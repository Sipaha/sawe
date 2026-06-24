//! Conflict data model + parsers for `git ls-files -u --stage` output and
//! `.git/<op>_HEAD` files.
//!
//! Pulls the three index stages (`:1:`, `:2:`, `:3:`) for each conflicted path
//! via `git show`. Working-copy text is read from disk separately by the
//! caller — this module is purely the index/HEAD-derived state.

use anyhow::{Context as _, Result, anyhow};
use git::repository::RepoPath;
use std::path::{Path, PathBuf};
use util::command::{Stdio, new_command};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictedFile {
    pub path: RepoPath,
    pub has_base: bool,
    pub has_ours: bool,
    pub has_theirs: bool,
    pub is_binary: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreeWayContent {
    pub base: Option<String>,
    pub ours: Option<String>,
    pub theirs: Option<String>,
    pub working: String,
}

/// Operation currently in progress in the working tree, deduced from the
/// presence of `.git/<op>_HEAD` files. The presence of any of these implies
/// `git <op> --continue|--abort|--skip` is the right finalizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProgressOp {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl InProgressOp {
    /// CLI subcommand name (for `git <subcmd> --continue` etc.). `Rebase`
    /// uses `rebase`, but in practice the resolver also has to consider
    /// REBASE_MERGE state — see [`detect_in_progress_op`].
    pub fn cli_subcommand(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
        }
    }

    /// Whether `git <op> --skip` is allowed. `merge` does not support
    /// `--skip`; `cherry-pick`, `rebase`, `revert` do.
    pub fn supports_skip(self) -> bool {
        !matches!(self, Self::Merge)
    }
}

/// Inspects the .git directory of the repo at `repo_path` for in-progress
/// operation markers. Returns `None` if no operation is in progress.
///
/// Looks for `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, and the
/// `rebase-apply` / `rebase-merge` directories used by `git rebase`.
pub fn detect_in_progress_op(git_dir: &Path) -> Option<InProgressOp> {
    if git_dir.join("CHERRY_PICK_HEAD").exists() {
        return Some(InProgressOp::CherryPick);
    }
    if git_dir.join("REVERT_HEAD").exists() {
        return Some(InProgressOp::Revert);
    }
    if git_dir.join("rebase-apply").exists() || git_dir.join("rebase-merge").exists() {
        return Some(InProgressOp::Rebase);
    }
    if git_dir.join("MERGE_HEAD").exists() {
        return Some(InProgressOp::Merge);
    }
    None
}

/// Parse the output of `git ls-files -u --stage`. Each entry is one stage;
/// a single conflicted path appears once per stage that exists (1=base,
/// 2=ours, 3=theirs). Output format:
///
/// ```text
/// <mode> SP <oid> SP <stage> TAB <path> NL
/// ```
///
/// `--stage` switches `<oid>` to the object id; the unstaged form uses `-`.
/// We always pass `--stage` so the OID column is reliable.
pub fn parse_ls_files_unmerged(stdout: &str) -> Result<Vec<ConflictedFile>> {
    use std::collections::BTreeMap;

    let mut by_path: BTreeMap<String, [bool; 4]> = BTreeMap::new();

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let (head, path) = line
            .split_once('\t')
            .context("ls-files -u line missing TAB separator")?;
        let cols: Vec<&str> = head.split_ascii_whitespace().collect();
        if cols.len() < 3 {
            return Err(anyhow!("ls-files -u line malformed: {line:?}"));
        }
        let stage: usize = cols[2].parse().context("invalid stage column")?;
        if stage > 3 {
            continue;
        }
        let entry = by_path.entry(path.to_string()).or_insert([false; 4]);
        entry[stage] = true;
    }

    let mut out = Vec::with_capacity(by_path.len());
    for (path_str, stages) in by_path {
        let path = RepoPath::new(&path_str)?;
        out.push(ConflictedFile {
            path,
            has_base: stages[1],
            has_ours: stages[2],
            has_theirs: stages[3],
            is_binary: false,
        });
    }
    Ok(out)
}

/// Heuristic Git uses internally: a NUL byte in the first 8KB classifies the
/// blob as binary. Run on whichever side has content (we OR across all
/// available sides — if any side is binary, treat the conflict as binary).
pub fn looks_binary(bytes: &[u8]) -> bool {
    let scan = bytes.len().min(8192);
    let head: &[u8] = &bytes[..scan];
    head.contains(&0)
}

/// Async wrapper that invokes `git ls-files -u --stage` against `work_dir`.
/// Returns the parsed list of conflicted files, with `is_binary` populated
/// by inspecting the OID of stage 2 (ours) — falling back to stage 1 or 3.
pub async fn list_conflicts_async(work_dir: &Path) -> Result<Vec<ConflictedFile>> {
    let stdout = run_git(work_dir, &["ls-files", "-u", "--stage"]).await?;
    let mut entries = parse_ls_files_unmerged(&stdout)?;
    for entry in &mut entries {
        let path_str = entry.path.as_std_path().to_string_lossy().into_owned();
        let probe_stage = if entry.has_ours {
            2
        } else if entry.has_theirs {
            3
        } else if entry.has_base {
            1
        } else {
            continue;
        };
        let spec = format!(":{probe_stage}:{path_str}");
        match run_git_bytes(work_dir, &["show", &spec]).await {
            Ok(bytes) => entry.is_binary = looks_binary(&bytes),
            Err(err) => log::debug!("binary probe for {spec} failed: {err}"),
        }
    }
    Ok(entries)
}

/// Pull the three index sides + working text for `path`. Stages that don't
/// exist (e.g. add/add — no base) are returned as `None`. Working text is
/// read from `work_dir.join(path)`; missing working file is an error
/// because the resolver only opens for files Git has placed in the working
/// tree.
pub async fn load_three_way_async(work_dir: &Path, path: &RepoPath) -> Result<ThreeWayContent> {
    let path_str = path.as_std_path().to_string_lossy().into_owned();

    let base = git_show_text(work_dir, 1, &path_str).await;
    let ours = git_show_text(work_dir, 2, &path_str).await;
    let theirs = git_show_text(work_dir, 3, &path_str).await;

    let working_path = work_dir.join(path.as_std_path());
    let working = std::fs::read_to_string(&working_path)
        .with_context(|| format!("read working tree {}", working_path.display()))?;

    Ok(ThreeWayContent {
        base: base.ok(),
        ours: ours.ok(),
        theirs: theirs.ok(),
        working,
    })
}

async fn git_show_text(work_dir: &Path, stage: u8, path_str: &str) -> Result<String> {
    let spec = format!(":{stage}:{path_str}");
    let bytes = run_git_bytes(work_dir, &["show", &spec]).await?;
    if looks_binary(&bytes) {
        return Err(anyhow!("blob is binary: {spec}"));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn run_git(work_dir: &Path, args: &[&str]) -> Result<String> {
    let bytes = run_git_bytes(work_dir, args).await?;
    Ok(String::from_utf8(bytes)?)
}

async fn run_git_bytes(work_dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let work_dir_buf: PathBuf = work_dir.to_path_buf();
    let mut command = new_command("git");
    command.current_dir(&work_dir_buf);
    command.args(args);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await.context("running `git`")?;
    if !output.status.success() {
        return Err(anyhow!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_stage_conflict() {
        let raw = "\
100644 0000000000000000000000000000000000000001 1\tsrc/a.rs
100644 0000000000000000000000000000000000000002 2\tsrc/a.rs
100644 0000000000000000000000000000000000000003 3\tsrc/a.rs
";
        let parsed = parse_ls_files_unmerged(raw).expect("parse");
        assert_eq!(parsed.len(), 1);
        let entry = &parsed[0];
        assert_eq!(entry.path.as_std_path().to_string_lossy(), "src/a.rs");
        assert!(entry.has_base);
        assert!(entry.has_ours);
        assert!(entry.has_theirs);
    }

    #[test]
    fn parses_addadd_conflict_without_base() {
        let raw = "\
100644 deadbeef 2\tnew.txt
100644 cafef00d 3\tnew.txt
";
        let parsed = parse_ls_files_unmerged(raw).expect("parse");
        assert_eq!(parsed.len(), 1);
        assert!(!parsed[0].has_base);
        assert!(parsed[0].has_ours);
        assert!(parsed[0].has_theirs);
    }

    #[test]
    fn parses_modify_delete_conflict() {
        let raw = "\
100644 1111111 1\tgone.txt
100644 2222222 2\tgone.txt
";
        let parsed = parse_ls_files_unmerged(raw).expect("parse");
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].has_base);
        assert!(parsed[0].has_ours);
        assert!(!parsed[0].has_theirs);
    }

    #[test]
    fn binary_detection_matches_git_heuristic() {
        let mostly_text = b"hello world\nfoo\nbar\n".to_vec();
        assert!(!looks_binary(&mostly_text));
        let with_null = {
            let mut v = b"hello".to_vec();
            v.push(0);
            v.extend_from_slice(b"world");
            v
        };
        assert!(looks_binary(&with_null));
    }

    #[test]
    fn detect_in_progress_op_finds_cherry_pick() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CHERRY_PICK_HEAD"), "abc").unwrap();
        assert_eq!(
            detect_in_progress_op(dir.path()),
            Some(InProgressOp::CherryPick)
        );
    }

    #[test]
    fn detect_in_progress_op_finds_rebase_merge() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("rebase-merge")).unwrap();
        assert_eq!(
            detect_in_progress_op(dir.path()),
            Some(InProgressOp::Rebase)
        );
    }

    #[test]
    fn detect_in_progress_op_returns_none_when_clean() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_in_progress_op(dir.path()), None);
    }
}
