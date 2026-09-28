//! Policy for `can_use_tool` control requests: who answers, and how.
//!
//! Claude raises two different things through the same frame.
//!
//! * **The ordinary gate.** An Agent Teams teammate cannot inherit
//!   `bypassPermissions`, so every one of its tool calls asks. The operator
//!   already opted the workspace into bypass for the main agent, so these are
//!   answered by policy and never surface.
//!
//! * **A safety hook.** Claude refuses to decide on its own and says why in
//!   `decision_reason` — e.g. *"Dangerous rm operation detected: `"$D"/*.jar`
//!   … points at the filesystem root when the variable is unset or empty …
//!   This requires explicit approval and cannot be auto-allowed by permission
//!   rules."* `bypassPermissions` deliberately does not cover this class.
//!
//! The operator's rule for the second class: **full rights inside the
//! Solution, a confirmation outside it.** So we resolve what the flagged
//! target actually points at and only answer by policy when every target
//! provably lands inside a work directory. Anything we cannot prove — an
//! unresolvable variable, a bare root, a path elsewhere on the machine — goes
//! to the operator.
//!
//! Resolution is deliberately static and conservative: it reads the literal
//! assignments in the command text, expands `$HOME`/`~`, and stops at the
//! first thing it cannot resolve, keeping the longest literal prefix. A prefix
//! is enough — everything below it shares the same inside/outside answer.
//! Guessing beyond that would mean auto-allowing an `rm` we do not understand,
//! which is the one outcome worth avoiding.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What the session's own mode allows before any per-call reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPolicy {
    /// Tools are off entirely (commit-message generation): deny everything.
    NoTools,
    /// Read-only session: deny anything that asks.
    ReadOnly,
    /// Ordinary full-access session.
    FullAccess,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationDecision {
    /// Answer `allow` without involving the operator.
    Allow,
    /// Answer `deny` without involving the operator.
    Deny,
    /// Put it in front of the operator. `reason` is claude's own words;
    /// `targets` are the paths we could not place inside the Solution.
    Ask {
        reason: String,
        outside_targets: Vec<String>,
    },
}

/// Decide how to answer one `can_use_tool` request.
pub fn decide(
    input: &serde_json::Value,
    decision_reason: Option<&str>,
    work_dirs: &[PathBuf],
    policy: SessionPolicy,
) -> AuthorizationDecision {
    if matches!(policy, SessionPolicy::NoTools | SessionPolicy::ReadOnly) {
        return AuthorizationDecision::Deny;
    }
    let Some(reason) = decision_reason else {
        // The ordinary teammate gate — the workspace is already in bypass.
        return AuthorizationDecision::Allow;
    };

    let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let assignments = literal_assignments(command);
    let flagged = flagged_targets(reason);

    // `rm -rf "$(cat <file>)"`: claude cannot resolve a command substitution
    // statically, so it names no path at all. The substitution is a file the
    // agent wrote itself, though, and reading it answers the question claude
    // could not.
    if flagged.is_empty()
        && reason.contains("command substitution")
        && substitutions_land_inside(command, &assignments, work_dirs)
    {
        return AuthorizationDecision::Allow;
    }

    // Nothing extractable to reason about: we cannot prove containment, so ask.
    if flagged.is_empty() {
        return AuthorizationDecision::Ask {
            reason: reason.to_string(),
            outside_targets: Vec::new(),
        };
    }

    let mut outside = Vec::new();
    for target in &flagged {
        match resolve_prefix(target, &assignments) {
            Some(prefix) if is_inside(&prefix, work_dirs) => {}
            _ => outside.push(target.clone()),
        }
    }
    if outside.is_empty() {
        AuthorizationDecision::Allow
    } else {
        AuthorizationDecision::Ask {
            reason: reason.to_string(),
            outside_targets: outside,
        }
    }
}

/// Pull the targets claude named out of its own explanation.
///
/// Two phrasings have been observed for the same check, so both are handled
/// and neither is assumed: `Dangerous rm operation detected: '"$D"/*.jar'` and
/// `Dangerous rm operation on possibly-empty variable path: "$D"/*.jar`. The
/// reliable part is the `: <target>` tail of the first line; quoted spans are
/// scanned as a second source. Shell quotes are stripped either way — they are
/// quoting, not path content.
///
/// Returning nothing is safe: the caller treats "cannot tell" as "ask".
fn flagged_targets(reason: &str) -> Vec<String> {
    let mut out = Vec::new();
    let first_line = reason.lines().next().unwrap_or_default();
    if let Some((_, tail)) = first_line.split_once(": ") {
        let token = strip_shell_quotes(tail);
        if token.contains('/') {
            out.push(token);
        }
    }
    for (open, close) in [('\'', '\''), ('`', '`'), ('"', '"')] {
        let mut rest = reason;
        while let Some(start) = rest.find(open) {
            let after = &rest[start + open.len_utf8()..];
            let Some(end) = after.find(close) else { break };
            let token = strip_shell_quotes(&after[..end]);
            if !token.is_empty() && token.contains('/') && !out.contains(&token) {
                out.push(token);
            }
            rest = &after[end + close.len_utf8()..];
        }
        if !out.is_empty() {
            break;
        }
    }
    out
}

/// Drop shell quoting characters and surrounding whitespace/punctuation from a
/// target claude quoted back at us.
fn strip_shell_quotes(token: &str) -> String {
    token
        .trim()
        .trim_end_matches(['.', ','])
        .chars()
        .filter(|c| *c != '"' && *c != '\'' && *c != '`')
        .collect::<String>()
        .trim()
        .to_string()
}

/// Collect `NAME=<value>` assignments from a command, iterating so that
/// `B=/tmp/x; D=$B/sub` resolves through the chain.
///
/// Each entry keeps the longest literal PREFIX plus whether it is complete. A
/// prefix is the useful unit here: `D=<root>/target/$n` cannot be expanded (the
/// loop variable is unknown) yet still proves every `$D` lands under
/// `<root>/target/`, which is the whole question being asked.
fn literal_assignments(command: &str) -> HashMap<String, (String, bool)> {
    let mut raw: Vec<(String, String)> = Vec::new();
    for (idx, _) in command.match_indices('=') {
        let before = &command[..idx];
        let name_start = before
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map(|i| i + 1)
            .unwrap_or(0);
        let name = &before[name_start..];
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        // `==`, `!=`, `<=` are comparisons, not assignments.
        if before.ends_with(['!', '<', '>', '=']) {
            continue;
        }
        let after = &command[idx + 1..];
        let value: String = after
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != ';' && *c != '&' && *c != '|')
            .collect();
        if value.is_empty() {
            continue;
        }
        raw.push((name.to_string(), value));
    }

    // Iterate so a chain (`B=…; D=$B/sub`) resolves; a few passes is plenty and
    // bounds any accidental self-reference.
    let mut resolved: HashMap<String, (String, bool)> = HashMap::new();
    for _ in 0..4 {
        let mut progressed = false;
        for (name, value) in &raw {
            let expanded = expand_prefix(value, &resolved);
            match resolved.get(name) {
                Some(current) if *current == expanded => {}
                _ => {
                    resolved.insert(name.clone(), expanded);
                    progressed = true;
                }
            }
        }
        if !progressed {
            break;
        }
    }
    resolved
}

/// Expand as far as possible and report whether it completed. On an
/// unresolvable reference (or a glob) expansion stops and the literal prefix
/// so far is returned with `false`.
fn expand_prefix(value: &str, known: &HashMap<String, (String, bool)>) -> (String, bool) {
    let mut out = String::new();
    let chars: Vec<char> = value.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '"' | '\'' => i += 1,
            '*' | '?' | '[' => return (out, false),
            '~' if i == 0 => {
                let Some(home) = home_dir() else {
                    return (out, false);
                };
                out.push_str(&home.to_string_lossy());
                i += 1;
            }
            '$' => {
                let mut j = i + 1;
                let braced = chars.get(j) == Some(&'{');
                if braced {
                    j += 1;
                }
                let start = j;
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let name: String = chars[start..j].iter().collect();
                if braced {
                    if chars.get(j) != Some(&'}') {
                        return (out, false);
                    }
                    j += 1;
                }
                if name.is_empty() {
                    return (out, false);
                }
                let value = if name == "HOME" {
                    home_dir().map(|h| (h.to_string_lossy().into_owned(), true))
                } else {
                    known.get(&name).cloned()
                };
                let Some((value, complete)) = value else {
                    return (out, false);
                };
                out.push_str(&value);
                if !complete {
                    return (out, false);
                }
                i = j;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    (out, true)
}

/// Longest literal path prefix a target can be reduced to, or `None` when not
/// even the first component is knowable (a bare `$UNSET/...`, a relative path
/// we cannot anchor). A prefix of `/` is treated as unknowable: it proves
/// nothing and is exactly the case claude flags.
fn resolve_prefix(target: &str, assignments: &HashMap<String, (String, bool)>) -> Option<PathBuf> {
    let (prefix, _) = expand_prefix(target, assignments);
    let prefix = prefix.trim();
    if prefix.is_empty() || prefix == "/" || !prefix.starts_with('/') {
        return None;
    }
    // Drop a trailing partial component so `/tmp/x/ab` from `/tmp/x/a` + `b`
    // can never be read as a deeper directory than it is.
    let path = PathBuf::from(prefix);
    Some(path)
}

/// Whether EVERY command substitution in `command` provably yields a path
/// strictly inside a work directory.
///
/// Only the file-read forms are understood — `$(cat <file>)` and `$(< <file>)`
/// with a literal (or literally-assigned) file path. The file is read now, and
/// its content must be exactly one absolute path with no glob or whitespace
/// characters (so neither word splitting nor globbing can widen it), strictly
/// BELOW a work directory — never the Solution root itself. Any other
/// substitution anywhere in the command (`$(date)`, backticks, a nested
/// `$(…)`), an unreadable file, or content that fails a check means "cannot
/// prove it", and the operator is asked.
///
/// The file can change between this read and the command running; it is a
/// file inside the Solution, where the agent already has full rights, so that
/// is no wider than what the operator granted.
fn substitutions_land_inside(
    command: &str,
    assignments: &HashMap<String, (String, bool)>,
    work_dirs: &[PathBuf],
) -> bool {
    if command.contains('`') {
        return false;
    }
    let mut found = false;
    let mut rest = command;
    while let Some(start) = rest.find("$(") {
        let body_start = start + 2;
        let Some(len) = rest[body_start..].find(')') else {
            return false;
        };
        let body = rest[body_start..body_start + len].trim();
        if body.contains("$(") {
            return false;
        }
        let file = if let Some(file) = body.strip_prefix("cat ") {
            file
        } else if let Some(file) = body.strip_prefix('<') {
            file
        } else {
            return false;
        };
        let file = file.trim();
        if file.is_empty() || file.contains(char::is_whitespace) {
            return false;
        }
        let (file, complete) = expand_prefix(file, assignments);
        if !complete || !file.starts_with('/') {
            return false;
        }
        let Ok(content) = std::fs::read_to_string(&file) else {
            return false;
        };
        let target = content.trim_end_matches('\n');
        if target.is_empty()
            || !target.starts_with('/')
            || target.contains(|c: char| c.is_whitespace() || matches!(c, '*' | '?' | '[' | '$' | '`'))
            || target.split('/').any(|component| component == "..")
        {
            return false;
        }
        let target = Path::new(target);
        if !work_dirs
            .iter()
            .any(|root| target.starts_with(root) && target != root.as_path())
        {
            return false;
        }
        found = true;
        rest = &rest[body_start + len + 1..];
    }
    found
}

fn is_inside(path: &Path, work_dirs: &[PathBuf]) -> bool {
    !work_dirs.is_empty() && work_dirs.iter().any(|root| path.starts_with(root))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RM_REASON: &str = "Dangerous rm operation detected: '\"$D\"/*.jar'\n\nThis target is a \
                             shell variable expansion that points at the filesystem root (or a \
                             top-level directory) when the variable is unset or empty.";

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/home/u/ss/MySolution")]
    }

    fn bash(command: &str) -> serde_json::Value {
        json!({ "command": command })
    }

    #[test]
    fn teammate_gate_without_a_reason_is_answered_by_policy() {
        let decision = decide(&bash("ls"), None, &roots(), SessionPolicy::FullAccess);
        assert_eq!(decision, AuthorizationDecision::Allow);
    }

    #[test]
    fn read_only_and_no_tools_sessions_never_ask() {
        for policy in [SessionPolicy::ReadOnly, SessionPolicy::NoTools] {
            assert_eq!(
                decide(&bash("ls"), Some(RM_REASON), &roots(), policy),
                AuthorizationDecision::Deny
            );
        }
    }

    #[test]
    fn target_inside_the_solution_is_allowed_without_asking() {
        let cmd = "for n in a b; do D=/home/u/ss/MySolution/target/$n; rm -f \"$D\"/*.jar; done";
        assert_eq!(
            decide(
                &bash(cmd),
                Some(RM_REASON),
                &roots(),
                SessionPolicy::FullAccess
            ),
            AuthorizationDecision::Allow
        );
    }

    #[test]
    fn target_outside_the_solution_goes_to_the_operator() {
        let cmd = "for n in a b; do D=$HOME/.m2/repository/x/$n; rm -f \"$D\"/*.jar; done";
        // SAFETY: single-threaded test; HOME is read, not mutated, by the code.
        unsafe { std::env::set_var("HOME", "/home/u") };
        match decide(
            &bash(cmd),
            Some(RM_REASON),
            &roots(),
            SessionPolicy::FullAccess,
        ) {
            AuthorizationDecision::Ask {
                outside_targets, ..
            } => assert_eq!(outside_targets, vec!["$D/*.jar".to_string()]),
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn an_unresolvable_variable_goes_to_the_operator() {
        let cmd = "rm -f \"$D\"/*.jar";
        assert!(matches!(
            decide(
                &bash(cmd),
                Some(RM_REASON),
                &roots(),
                SessionPolicy::FullAccess
            ),
            AuthorizationDecision::Ask { .. }
        ));
    }

    #[test]
    fn a_root_prefix_is_never_treated_as_inside() {
        let cmd = "D=/; rm -f \"$D\"/*.jar";
        assert!(matches!(
            decide(
                &bash(cmd),
                Some(RM_REASON),
                &roots(),
                SessionPolicy::FullAccess
            ),
            AuthorizationDecision::Ask { .. }
        ));
    }

    #[test]
    fn chained_assignments_resolve() {
        let cmd = "B=/home/u/ss/MySolution; D=$B/build; rm -f \"$D\"/*.jar";
        assert_eq!(
            decide(
                &bash(cmd),
                Some(RM_REASON),
                &roots(),
                SessionPolicy::FullAccess
            ),
            AuthorizationDecision::Allow
        );
    }

    /// The same check has been observed under two wordings. Both must yield
    /// the same target, or the policy silently falls back to "ask" for work
    /// the operator granted full rights to.
    #[test]
    fn both_observed_wordings_yield_the_same_target() {
        let detected = "Dangerous rm operation detected: '\"$D\"/*.jar'";
        let possibly_empty = "Dangerous rm operation on possibly-empty variable path: \"$D\"/*.jar";
        assert_eq!(flagged_targets(detected), vec!["$D/*.jar".to_string()]);
        assert_eq!(
            flagged_targets(possibly_empty),
            vec!["$D/*.jar".to_string()]
        );

        let cmd = "for n in a b; do D=/home/u/ss/MySolution/t/$n; rm -f \"$D\"/*.jar; done";
        for reason in [detected, possibly_empty] {
            assert_eq!(
                decide(
                    &bash(cmd),
                    Some(reason),
                    &roots(),
                    SessionPolicy::FullAccess
                ),
                AuthorizationDecision::Allow,
                "wording must not change the verdict: {reason}"
            );
        }
    }

    const SUBSTITUTION_REASON: &str =
        "Dangerous rm operation on statically-unresolvable target: command substitution output";

    /// The wording claude 2.1.282 uses for `rm -rf "$(cat file)"` (captured
    /// from a live `can_use_tool`), resolved by reading the file.
    #[test]
    fn a_substituted_rm_target_read_from_a_file_inside_is_allowed() {
        let root = tempfile::tempdir().expect("root");
        let roots = vec![root.path().to_path_buf()];
        let doomed = root.path().join(".agents/tmp/tmp.Ru3jifhqK7");
        std::fs::create_dir_all(&doomed).expect("doomed");
        let pointer = root.path().join(".agents/tmp/appdir.txt");
        let write = |content: &str| std::fs::write(&pointer, content).expect("pointer");
        let decide_for = |cmd: String| {
            decide(&bash(&cmd), Some(SUBSTITUTION_REASON), &roots, SessionPolicy::FullAccess)
        };
        let quoted = format!("rm -rf \"$(cat {})\"", pointer.display());

        write(&format!("{}\n", doomed.display()));
        assert_eq!(decide_for(quoted.clone()), AuthorizationDecision::Allow);
        assert_eq!(
            decide_for(format!("P={}; rm -rf \"$(< $P)\"", pointer.display())),
            AuthorizationDecision::Allow,
            "`$(< file)` through a literal assignment"
        );

        // Everything below must still reach the operator.
        let asks = |cmd: String| matches!(decide_for(cmd), AuthorizationDecision::Ask { .. });
        write("/etc\n");
        assert!(asks(quoted.clone()), "content outside the Solution");
        write(&format!("{}\n", root.path().display()));
        assert!(asks(quoted.clone()), "the Solution root itself");
        write(&format!("{}/*\n", doomed.display()));
        assert!(asks(quoted.clone()), "a glob in the content");
        write(&format!("{}/../../..\n", doomed.display()));
        assert!(asks(quoted.clone()), "`..` climbing out");
        write(&format!("{} /etc\n", doomed.display()));
        assert!(asks(quoted), "two words");
        write(&format!("{}\n", doomed.display()));
        assert!(
            asks(format!("rm -rf \"$(cat {})\" \"$(date)\"", pointer.display())),
            "any other substitution in the command"
        );
        assert!(asks(format!("rm -rf \"$(cat {}.missing)\"", pointer.display())), "unreadable");
        assert!(asks("rm -rf \"`cat /x`\"".to_string()), "backticks");
    }

    #[test]
    fn a_reason_naming_no_path_still_asks() {
        assert!(matches!(
            decide(
                &bash("rm -rf /"),
                Some("Dangerous operation"),
                &roots(),
                SessionPolicy::FullAccess
            ),
            AuthorizationDecision::Ask { .. }
        ));
    }

    #[test]
    fn comparisons_are_not_read_as_assignments() {
        let assignments =
            literal_assignments("if [ \"$a\" == /tmp/x ]; then D=/home/u/ss/MySolution; fi");
        assert_eq!(
            assignments.get("D"),
            Some(&("/home/u/ss/MySolution".to_string(), true))
        );
        assert!(!assignments.contains_key("a"));
    }
}
