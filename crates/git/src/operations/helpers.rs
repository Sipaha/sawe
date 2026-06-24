//! Pure-stdlib subcommand handlers for `spk-editor --git-rebase-helper` and
//! `spk-editor --git-message-set`. Called from `crates/zed/src/main.rs` argv
//! handling. No GPUI / no editor init — the helper binary path is invoked
//! many times per interactive rebase by `git`, so fast exit is critical.
//!
//! Inputs are read from a session directory minted by the parent process
//! (`run_rebase` in `operations::rebase`). Path validation rules (P-11):
//!
//! - Session-id is required via `SPK_GIT_HELPER_SESSION` env var and must be
//!   exactly 32 lowercase-hex characters (UUIDv4 simple form).
//! - Session directory must already exist at
//!   `<paths::temp_dir()>/git-helper/<session-id>/`. On Unix it must have
//!   permissions `0700` and be owned by the current user.
//! - For `--git-rebase-helper`: input is `<session-dir>/todo.txt`. Output is
//!   the `<todo-path>` argument git provides; it must be a regular file
//!   (not a symlink) inside a real on-disk path.
//! - For `--git-message-set`: token must match `^[a-z0-9]{16,32}$`. Input is
//!   `<session-dir>/messages/<token>.txt`. The helper invokes
//!   `git commit --amend -F <message-path>` in the current working
//!   directory (= the rebase worktree).

use std::ffi::OsStr;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const SESSION_ENV: &str = "SPK_GIT_HELPER_SESSION";
const SESSION_ID_LEN: usize = 32;
const HELPER_SUBDIR: &str = "git-helper";
const TODO_FILE: &str = "todo.txt";
const MESSAGES_SUBDIR: &str = "messages";

/// Errors emitted by the helper subcommands. Each carries enough context to
/// land verbatim in stderr.
#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    #[error("missing required env var {0}")]
    MissingEnv(&'static str),
    #[error("invalid session id {0:?}: must be {SESSION_ID_LEN} lowercase-hex characters")]
    InvalidSessionId(String),
    #[error("session directory not found: {0}")]
    SessionDirMissing(PathBuf),
    #[error("session directory has wrong permissions: {0} (want 0700)")]
    SessionDirPerms(PathBuf),
    #[error("session input file missing: {0}")]
    InputMissing(PathBuf),
    #[error("invalid token {0:?}: must be 16..=32 lowercase-hex characters")]
    InvalidToken(String),
    #[error("output path is not writable file: {0}")]
    InvalidOutputPath(PathBuf),
    #[error("io error on {path}: {err}")]
    Io {
        path: PathBuf,
        #[source]
        err: std::io::Error,
    },
    #[error("git commit --amend failed (exit {code}): {stderr}")]
    GitFailed { code: i32, stderr: String },
    #[error("spawning git: {0}")]
    Spawn(#[source] std::io::Error),
}

/// Implementation of `spk-editor --git-rebase-helper <todo-path>`. Reads
/// `<session-dir>/todo.txt`, validates it parses as a git rebase todo, and
/// overwrites `todo_path` in place.
pub fn rebase_helper_main(todo_path: &Path) -> Result<(), HelperError> {
    let session_id = read_session_id()?;
    let session_dir = resolve_session_dir(&session_id)?;
    let input_path = session_dir.join(TODO_FILE);
    if !input_path.is_file() {
        return Err(HelperError::InputMissing(input_path));
    }

    // Read & validate the prepared todo before clobbering git's file.
    let body = read_to_string(&input_path)?;
    validate_todo_body(&body)?;

    // The output path must be a regular file (git creates it before invoking
    // the editor). Symlinks would let an attacker redirect overwrites.
    let metadata = fs::symlink_metadata(todo_path).map_err(|err| HelperError::Io {
        path: todo_path.to_path_buf(),
        err,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(HelperError::InvalidOutputPath(todo_path.to_path_buf()));
    }

    fs::write(todo_path, body.as_bytes()).map_err(|err| HelperError::Io {
        path: todo_path.to_path_buf(),
        err,
    })?;
    Ok(())
}

/// Implementation of `spk-editor --git-message-set <token>`. Looks up the
/// pre-staged commit message at `<session-dir>/messages/<token>.txt` and
/// runs `git commit --amend -F <path>` in the current working directory.
pub fn message_set_main(token: &str) -> Result<(), HelperError> {
    if !is_valid_token(token) {
        return Err(HelperError::InvalidToken(token.to_string()));
    }
    let session_id = read_session_id()?;
    let session_dir = resolve_session_dir(&session_id)?;
    let messages_dir = session_dir.join(MESSAGES_SUBDIR);
    let mut message_path = messages_dir.join(format!("{token}.txt"));

    // Defence against an attacker-controlled token sneaking past the regex
    // (it can't, but a static check pins the policy): the resolved file must
    // remain inside `messages_dir`.
    if let Ok(canonical) = message_path.canonicalize() {
        if !canonical.starts_with(&messages_dir.canonicalize().unwrap_or(messages_dir.clone())) {
            return Err(HelperError::InvalidToken(token.to_string()));
        }
        message_path = canonical;
    }

    if !message_path.is_file() {
        return Err(HelperError::InputMissing(message_path));
    }

    // CWD is the rebase worktree (`git` sets it before running exec lines).
    let output = run_git_amend(&message_path)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(HelperError::GitFailed {
            code: output.status.code().unwrap_or(-1),
            stderr,
        });
    }
    Ok(())
}

fn read_session_id() -> Result<String, HelperError> {
    let raw = std::env::var(SESSION_ENV).map_err(|_| HelperError::MissingEnv(SESSION_ENV))?;
    if !is_valid_session_id(&raw) {
        return Err(HelperError::InvalidSessionId(raw));
    }
    Ok(raw)
}

fn resolve_session_dir(session_id: &str) -> Result<PathBuf, HelperError> {
    let helper_root = paths::temp_dir().join(HELPER_SUBDIR);
    let session_dir = helper_root.join(session_id);
    let metadata = fs::symlink_metadata(&session_dir).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            HelperError::SessionDirMissing(session_dir.clone())
        } else {
            HelperError::Io {
                path: session_dir.clone(),
                err,
            }
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(HelperError::SessionDirMissing(session_dir));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(HelperError::SessionDirPerms(session_dir));
        }
        let uid = metadata.uid();
        // SAFETY: getuid is async-signal-safe, no preconditions.
        let real_uid = unsafe { libc::getuid() };
        if uid != real_uid {
            return Err(HelperError::SessionDirPerms(session_dir));
        }
    }
    Ok(session_dir)
}

pub(crate) fn is_valid_session_id(s: &str) -> bool {
    s.len() == SESSION_ID_LEN && s.bytes().all(is_lowercase_hex)
}

pub(crate) fn is_valid_token(s: &str) -> bool {
    let n = s.len();
    (16..=32).contains(&n) && s.bytes().all(is_lowercase_hex)
}

fn is_lowercase_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

fn read_to_string(path: &Path) -> Result<String, HelperError> {
    let mut file = fs::File::open(path).map_err(|err| HelperError::Io {
        path: path.to_path_buf(),
        err,
    })?;
    let mut body = String::new();
    file.read_to_string(&mut body)
        .map_err(|err| HelperError::Io {
            path: path.to_path_buf(),
            err,
        })?;
    Ok(body)
}

/// Validate that `body` is a recognisable git rebase todo. Each non-empty,
/// non-comment line must look like `<verb> <hash> [args]` where `<verb>` is a
/// known rebase command (full word or abbreviation per git's parser) or
/// `exec` (whose body is arbitrary). Failure is surfaced via
/// [`HelperError::InvalidOutputPath`] of the input path.
pub(crate) fn validate_todo_body(body: &str) -> Result<(), HelperError> {
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let verb = parts.next().unwrap_or("");
        if !is_valid_verb(verb) {
            return Err(HelperError::Io {
                path: PathBuf::from("<todo>"),
                err: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown rebase verb: {verb:?}"),
                ),
            });
        }
        // `exec`/`x` can carry arbitrary commands; everything else needs a
        // sha-shaped argument (we don't enforce hex length — git accepts any
        // commit-ish that resolves; minimal check that something follows).
        if matches!(
            verb,
            "exec" | "x" | "label" | "l" | "reset" | "t" | "merge" | "m"
        ) {
            continue;
        }
        let rest = parts.next().unwrap_or("").trim();
        if rest.is_empty() {
            return Err(HelperError::Io {
                path: PathBuf::from("<todo>"),
                err: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("rebase verb {verb:?} requires an argument"),
                ),
            });
        }
    }
    Ok(())
}

fn is_valid_verb(verb: &str) -> bool {
    matches!(
        verb,
        "pick"
            | "p"
            | "reword"
            | "r"
            | "edit"
            | "e"
            | "squash"
            | "s"
            | "fixup"
            | "f"
            | "exec"
            | "x"
            | "drop"
            | "d"
            | "label"
            | "l"
            | "reset"
            | "t"
            | "merge"
            | "m"
            | "break"
            | "b"
            | "update-ref"
            | "u"
    )
}

#[allow(clippy::disallowed_methods)]
fn run_git_amend(message_path: &Path) -> Result<std::process::Output, HelperError> {
    Command::new("git")
        .args([
            OsStr::new("commit"),
            OsStr::new("--amend"),
            OsStr::new("-F"),
        ])
        .arg(message_path)
        .output()
        .map_err(HelperError::Spawn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialises tests that mutate process-global env vars. Cargo runs unit
    /// tests on multiple threads in the same process by default, so two tests
    /// touching `SPK_GIT_HELPER_SESSION` in parallel race.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn session_id_format_strict() {
        assert!(is_valid_session_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_valid_session_id("0123456789ABCDEF0123456789abcdef"));
        assert!(!is_valid_session_id("short"));
        assert!(!is_valid_session_id("0123456789abcdef0123456789abcde")); // 31
        assert!(!is_valid_session_id("0123456789abcdef0123456789abcdef0")); // 33
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("../etc/passwd../../../../../../../"));
    }

    #[test]
    fn token_format_strict() {
        assert!(is_valid_token("0123456789abcdef")); // 16
        assert!(is_valid_token("0123456789abcdef0123456789abcdef")); // 32
        assert!(!is_valid_token("0123456789abcde")); // 15
        assert!(!is_valid_token("0123456789ABCDEF"));
        assert!(!is_valid_token("../escape"));
    }

    #[test]
    fn rebase_helper_rejects_missing_session_env() {
        let _guard = ENV_GUARD.lock().expect("env guard");
        // SAFETY: ENV_GUARD serialises all tests in this module that touch
        // `SPK_GIT_HELPER_SESSION`, so no concurrent reader observes a torn
        // env state. Removing first so the env reflects "missing".
        unsafe {
            std::env::remove_var(SESSION_ENV);
        }
        let result = rebase_helper_main(Path::new("/dev/null"));
        match result {
            Err(HelperError::MissingEnv(name)) => assert_eq!(name, SESSION_ENV),
            other => panic!("expected MissingEnv, got {other:?}"),
        }
    }

    #[test]
    fn rebase_helper_rejects_invalid_session_id() {
        let _guard = ENV_GUARD.lock().expect("env guard");
        unsafe {
            std::env::set_var(SESSION_ENV, "not-hex");
        }
        let result = rebase_helper_main(Path::new("/dev/null"));
        unsafe {
            std::env::remove_var(SESSION_ENV);
        }
        match result {
            Err(HelperError::InvalidSessionId(s)) => assert_eq!(s, "not-hex"),
            other => panic!("expected InvalidSessionId, got {other:?}"),
        }
    }

    #[test]
    fn validate_todo_accepts_well_formed() {
        let body = "\
# this is a comment
pick deadbeef do thing
squash cafef00d another
exec spk-editor --git-message-set 0123456789abcdef0123456789abcdef
drop bbbbbbbb
";
        validate_todo_body(body).expect("ok");
    }

    #[test]
    fn validate_todo_rejects_unknown_verb() {
        let body = "punt deadbeef\n";
        let err = validate_todo_body(body).expect_err("must reject");
        match err {
            HelperError::Io { err, .. } => {
                assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
            }
            other => panic!("expected Io InvalidData, got {other:?}"),
        }
    }

    #[test]
    fn validate_todo_rejects_pick_without_sha() {
        let body = "pick\n";
        validate_todo_body(body).expect_err("must reject");
    }

    /// Even with a correctly formatted session id, a non-existent session dir
    /// must be rejected. (Path-traversal in the id is precluded by the regex,
    /// but this asserts the existence check still applies.)
    #[test]
    fn rebase_helper_rejects_missing_session_dir() {
        let _guard = ENV_GUARD.lock().expect("env guard");
        unsafe {
            std::env::set_var(SESSION_ENV, "00000000000000000000000000000000");
        }
        let result = rebase_helper_main(Path::new("/dev/null"));
        unsafe {
            std::env::remove_var(SESSION_ENV);
        }
        match result {
            // Either the dir doesn't exist (clean tester env) or somebody's
            // pre-created it with wrong perms; both are valid rejections.
            Err(HelperError::SessionDirMissing(_)) => {}
            Err(HelperError::SessionDirPerms(_)) => {}
            other => panic!("expected SessionDir{{Missing,Perms}}, got {other:?}"),
        }
    }
}
