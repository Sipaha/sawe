//! The editor-owned claude settings layer, handed to the subprocess as
//! `--settings <file>`.
//!
//! **Why `--settings` and not a fourth `--setting-sources` entry:**
//! `--setting-sources` accepts only `user`, `project`, `local` — there is no way
//! to name a directory of our own. `--settings` takes "a path to a settings JSON
//! file or an inline JSON string" and lands at the *command-line* precedence
//! tier, above local/project/user and below managed — so passing it does NOT
//! stop `--setting-sources user,project,local` from loading the user's own
//! settings.
//!
//! **The sharp edge:** "Values you set here override the same keys in your
//! settings.json files for this session. Keys you omit keep their file-based
//! values." `hooks` is one key — a bare `{"hooks": {ours}}` would silently
//! disable the user's own hooks. So we read the `hooks` object out of the three
//! sources we keep enabled, union them, and append ours.

use anyhow::{Context as _, Result};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// Escape hatch: set to any value to spawn `claude` with no `--settings` at all
/// (worktrees fall back to `<member>/.claude/worktrees/`, auto memory back to
/// the git-repo-root default).
pub const DISABLE_ENV_VAR: &str = "SAWE_CLAUDE_SETTINGS_DISABLED";

pub struct EditorClaudeSettings {
    /// `<solution_root>/.agents`
    pub agents_dir: PathBuf,
    /// The running editor binary — it is its own worktree hook
    /// (`--worktree-hook`). A dev build and a release build each hook
    /// themselves; a bare `sawe` on `$PATH` could be a different build entirely.
    pub editor_exe: PathBuf,
    /// The session's cwd (the member). Where project/local claude settings live.
    pub work_dir: PathBuf,
}

impl EditorClaudeSettings {
    pub fn worktrees_dir(&self) -> PathBuf {
        self.agents_dir.join("worktrees")
    }

    pub fn memory_dir(&self) -> PathBuf {
        self.agents_dir.join("memory")
    }

    pub fn to_json(&self) -> Value {
        let mut hooks = existing_hooks(&self.work_dir);
        for (event, mode) in [("WorktreeCreate", "create"), ("WorktreeRemove", "remove")] {
            let entry = json!({
                "hooks": [ { "type": "command", "command": self.hook_command(mode) } ]
            });
            let slot = hooks
                .entry(event.to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(entries) = slot.as_array_mut() {
                entries.push(entry);
            }
        }
        json!({
            "autoMemoryDirectory": self.memory_dir().to_string_lossy(),
            "hooks": Value::Object(hooks),
        })
    }

    pub fn write_to(&self, path: &Path) -> Result<()> {
        // claude does not create `autoMemoryDirectory` for us, and the hook's
        // base dir has to exist before the first `git worktree add`.
        for dir in [self.memory_dir(), self.worktrees_dir()] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let body = serde_json::to_vec_pretty(&self.to_json())
            .context("serializing the editor claude settings")?;
        std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
    }

    fn hook_command(&self, mode: &str) -> String {
        format!(
            "{} --worktree-hook {mode} --worktree-base {}",
            shell_quote(&self.editor_exe.to_string_lossy()),
            shell_quote(&self.worktrees_dir().to_string_lossy()),
        )
    }
}

/// `<runtime>/solutions/<id>/claude-settings.json` — beside that Solution's MCP
/// socket ([`editor_mcp::solution_socket_path`]), so its whole runtime footprint
/// is one directory the editor already creates, sweeps and drops.
pub fn settings_path(solution_id: i64) -> PathBuf {
    editor_mcp::runtime_dir()
        .join("solutions")
        .join(solution_id.to_string())
        .join("claude-settings.json")
}

/// The union of the `hooks` object across the three sources we keep enabled via
/// `--setting-sources user,project,local`. Re-emitting them is what keeps the
/// user's hooks alive under `--settings`' key-level override.
fn existing_hooks(work_dir: &Path) -> Map<String, Value> {
    let user_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| util::paths::home_dir().join(".claude"));
    let sources = [
        user_dir.join("settings.json"),
        work_dir.join(".claude").join("settings.json"),
        work_dir.join(".claude").join("settings.local.json"),
    ];

    let mut merged: Map<String, Value> = Map::new();
    for source in sources {
        let Ok(raw) = std::fs::read_to_string(&source) else {
            continue;
        };
        let value: Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(error) => {
                // claude itself tolerates a broken settings file; dropping the
                // whole layer over one would be worse than losing its hooks.
                log::warn!(
                    "claude settings: ignoring unparseable {}: {error}",
                    source.display()
                );
                continue;
            }
        };
        let Some(hooks) = value.get("hooks").and_then(Value::as_object) else {
            continue;
        };
        for (event, entries) in hooks {
            let Some(entries) = entries.as_array() else {
                continue;
            };
            let slot = merged
                .entry(event.clone())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(existing) = slot.as_array_mut() {
                for entry in entries {
                    // The same hook can appear in both user and project settings.
                    if !existing.contains(entry) {
                        existing.push(entry.clone());
                    }
                }
            }
        }
    }
    merged
}

/// A `command` hook is run through a shell, so a path with a space (or a `$`)
/// must be quoted.
#[cfg(not(windows))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// `cmd.exe` has no single-quote quoting; double quotes are the only form it
/// understands, and it has no escape for an embedded `"` — drop that rather than
/// emit a command that would silently mis-parse.
#[cfg(windows)]
fn shell_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', ""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(tmp: &Path) -> EditorClaudeSettings {
        EditorClaudeSettings {
            agents_dir: tmp.join("sol/.agents"),
            editor_exe: PathBuf::from("/opt/my apps/sawe"),
            work_dir: tmp.join("sol/member"),
        }
    }

    #[test]
    fn carries_the_two_hooks_and_the_memory_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let json = settings(tmp.path()).to_json();

        assert_eq!(
            json["autoMemoryDirectory"],
            serde_json::Value::String(
                tmp.path()
                    .join("sol/.agents/memory")
                    .to_string_lossy()
                    .into_owned()
            ),
            "auto memory is keyed by the git repo root (the member) by default, \
             so a member rename would strand it — pin it to the solution"
        );

        for (event, mode) in [("WorktreeCreate", "create"), ("WorktreeRemove", "remove")] {
            let entries = json["hooks"][event].as_array().expect("event entries");
            let ours = entries.last().expect("our entry");
            let command = ours["hooks"][0]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("{event} command"));
            assert!(
                command.contains(&format!("--worktree-hook {mode}")),
                "got: {command}"
            );
            assert!(
                command.contains("'/opt/my apps/sawe'"),
                "the hook command runs through a shell — quote the exe: {command}"
            );
            assert!(
                command.contains(".agents/worktrees'"),
                "worktrees must land under the solution root: {command}"
            );
            assert_eq!(ours["hooks"][0]["type"], "command");
        }
    }

    #[test]
    fn keeps_the_users_own_hooks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project_settings = tmp.path().join("sol/member/.claude");
        std::fs::create_dir_all(&project_settings).expect("mkdir");
        std::fs::write(
            project_settings.join("settings.json"),
            br#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo hi"}]}],
                         "WorktreeCreate":[{"hooks":[{"type":"command","command":"mine.sh"}]}]}}"#,
        )
        .expect("write");

        let json = settings(tmp.path()).to_json();

        // `--settings` overrides same-named keys wholesale, so re-emitting the
        // user's hooks is what keeps them alive.
        assert_eq!(
            json["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "echo hi"
        );
        assert_eq!(
            json["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"],
            "mine.sh"
        );
        let ours = json["hooks"]["WorktreeCreate"][1]["hooks"][0]["command"]
            .as_str()
            .expect("ours");
        assert!(ours.contains("--worktree-hook create"), "got: {ours}");
    }

    #[test]
    fn write_to_creates_the_agents_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let settings = settings(tmp.path());
        let path = tmp.path().join("state/solutions/7/claude-settings.json");

        settings.write_to(&path).expect("write");

        assert!(
            settings.memory_dir().is_dir(),
            "autoMemoryDirectory must exist"
        );
        assert!(settings.worktrees_dir().is_dir());
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
        assert_eq!(written, settings.to_json());
    }

    #[test]
    fn settings_path_sits_next_to_the_solution_socket() {
        let path = settings_path(7);
        assert!(
            path.ends_with("solutions/7/claude-settings.json"),
            "{}",
            path.display()
        );
        assert_eq!(
            path.parent(),
            editor_mcp::solution_socket_path(7).parent(),
            "the settings file must sit beside that Solution's socket"
        );
    }
}
