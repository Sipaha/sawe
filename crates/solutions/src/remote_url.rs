//! Normalisation + sanity checks for a user-supplied git remote URL.
//!
//! The catalog accepts a remote URL typed or pasted by hand, and a wrong one
//! is only discovered ~30 seconds later when `git clone` fails — after a
//! catalog row and a failed pending add already exist. A trailing `#` (the
//! bug that motivated this module: `https://host/group/repo#` cloned into a
//! redirect-to-sign-in failure) is a pure typo that costs nothing to catch at
//! entry.
//!
//! Scope is deliberately narrow. We normalise what is unambiguous and reject
//! only what cannot be a clone URL:
//!
//! * outer whitespace is trimmed, always;
//! * a URL **fragment** (`#…`) is stripped from URL-shaped inputs — git has no
//!   use for one in any transport it supports, so a trailing `#` or a `#L42`
//!   browse anchor is always a mistake;
//! * control characters and embedded whitespace are rejected — no valid remote
//!   contains a newline, and a URL-shaped input with a space in it is a
//!   truncated paste;
//! * a web *browse* URL (`…/-/tree/main`, `…/blob/main/…`) is rejected with the
//!   clone URL it should have been.
//!
//! Deliberately NOT done, and why:
//!
//! * **no scheme allow-list** — a bare filesystem path is a legitimate remote
//!   (the crate's own tests clone from a temp directory), so requiring
//!   `https://` / `ssh://` / `git@` would break real usage;
//! * **no host, DNS or reachability check** — that is what the clone is for,
//!   and a probe would be a second way to be wrong;
//! * **query strings are left alone** — `?ref=x` is not a typo class anyone
//!   here has hit, and git will report it plainly;
//! * **trailing `/` and `.git` are left alone** — both clone fine, and
//!   `same_remote` already folds them for duplicate detection.

/// Normalise `input`, or explain why it cannot be a git remote.
///
/// The error message is tagged `invalid_remote:` so MCP callers can branch on
/// it, matching the `duplicate_name:` / `duplicate_remote:` convention in
/// `store::catalog`; `humanize_catalog_error` strips the tag for the UI.
pub fn normalize_remote_url(input: &str) -> anyhow::Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("invalid_remote: the remote URL is empty");
    }
    if let Some(bad) = trimmed.chars().find(|c| c.is_control()) {
        anyhow::bail!(
            "invalid_remote: the remote URL contains a control character ({:?}) — paste it as a \
             single line",
            bad
        );
    }

    if trimmed.starts_with('#') {
        anyhow::bail!("invalid_remote: the remote URL is only a `#` fragment");
    }

    let url_shaped = looks_like_url(trimmed);
    if url_shaped && trimmed.chars().any(char::is_whitespace) {
        anyhow::bail!(
            "invalid_remote: the remote URL contains a space — it looks like a partial or \
             joined-up paste"
        );
    }

    // Only URL-shaped inputs get the fragment stripped: `#` is a legal (if
    // odd) character in a local directory name, and a bare path is a valid
    // remote.
    let without_fragment = if url_shaped {
        trimmed.split('#').next().unwrap_or(trimmed).trim_end()
    } else {
        trimmed
    };
    if without_fragment.is_empty() {
        anyhow::bail!("invalid_remote: the remote URL is only a `#` fragment");
    }

    if let Some(clone_url) = browse_url_suggestion(without_fragment) {
        anyhow::bail!(
            "invalid_remote: that is a web page URL, not a clone URL — try {clone_url} instead"
        );
    }

    Ok(without_fragment.to_string())
}

/// Does this input have the shape of a URL (scheme form or scp-like
/// `user@host:path`), as opposed to a plain filesystem path?
fn looks_like_url(candidate: &str) -> bool {
    candidate.contains("://") || is_scp_like(candidate)
}

/// `git@gitlab.example.com:group/repo.git` — a host before the first colon and
/// a relative path after it. Requires an `@` or a dot in the host part so a
/// Windows drive letter (`C:\src\repo`) is not mistaken for one.
fn is_scp_like(candidate: &str) -> bool {
    let Some((host, path)) = candidate.split_once(':') else {
        return false;
    };
    !host.is_empty()
        && !host.contains('/')
        && !host.contains('\\')
        && (host.contains('@') || host.contains('.'))
        && !path.starts_with('/')
        && !path.starts_with('\\')
}

/// If `candidate` is a forge *browse* URL, return the clone URL it was meant
/// to be. Recognises the two segments every GitLab / GitHub web link goes
/// through: GitLab's `/-/` separator and the `tree` / `blob` view segments.
fn browse_url_suggestion(candidate: &str) -> Option<String> {
    let (scheme, rest) = candidate.split_once("://")?;
    let (host, path) = rest.split_once('/')?;
    let segments: Vec<&str> = path.split('/').collect();
    let marker = segments
        .iter()
        .position(|segment| matches!(*segment, "-" | "tree" | "blob"))?;
    // A repository can legitimately be named `tree`; a view segment only
    // counts when something follows it (a ref, or a ref plus a path) and
    // something precedes it (the project path itself).
    if marker == 0 || marker + 1 >= segments.len() {
        return None;
    }
    Some(format!(
        "{scheme}://{host}/{}",
        segments[..marker].join("/")
    ))
}

#[cfg(test)]
mod tests {
    use super::normalize_remote_url;

    fn ok(input: &str) -> String {
        normalize_remote_url(input).expect("expected a valid remote URL")
    }

    fn err(input: &str) -> String {
        normalize_remote_url(input)
            .expect_err("expected a rejected remote URL")
            .to_string()
    }

    #[test]
    fn strips_a_trailing_fragment_from_url_shaped_input() {
        // The reported bug, verbatim.
        assert_eq!(
            ok("https://gitlab.citeck.ru/citeck-projects/citeck-hazelcast#"),
            "https://gitlab.citeck.ru/citeck-projects/citeck-hazelcast",
        );
        assert_eq!(ok("https://host/g/r.git#L42"), "https://host/g/r.git");
        assert_eq!(ok("git@host.example:g/r.git#"), "git@host.example:g/r.git");
    }

    #[test]
    fn trims_surrounding_whitespace_but_keeps_the_url() {
        assert_eq!(ok("  https://host/g/r.git \n"), "https://host/g/r.git");
        assert_eq!(
            ok("\tgit@host.example:g/r.git  "),
            "git@host.example:g/r.git"
        );
    }

    #[test]
    fn leaves_ordinary_urls_and_local_paths_untouched() {
        for input in [
            "https://host/g/r.git",
            "https://host/g/r/",
            "git@host.example:g/r",
            "ssh://git@host.example:22/g/r.git",
            "/tmp/some dir/bare.git",
            "/tmp/odd#name/bare.git",
            "C:\\src\\bare.git",
        ] {
            assert_eq!(ok(input), input, "{input} must survive normalisation");
        }
    }

    #[test]
    fn rejects_empty_control_characters_and_split_pastes() {
        assert!(err("   ").contains("empty"));
        assert!(err("https://host/g/r\n.git").contains("control character"));
        assert!(err("https://host/g/ r.git").contains("space"));
        assert!(err("#").contains("fragment"));
    }

    #[test]
    fn rejects_browse_urls_and_names_the_clone_url() {
        let message = err("https://gitlab.citeck.ru/citeck-projects/citeck-hazelcast/-/tree/main");
        assert!(
            message.contains("https://gitlab.citeck.ru/citeck-projects/citeck-hazelcast"),
            "the rejection must name the clone URL, got: {message}"
        );
        assert!(
            err("https://github.com/org/repo/blob/main/README.md")
                .contains("https://github.com/org/repo")
        );
        // Fragment first, then the browse check — a copied anchor link is
        // still recognisably a browse URL.
        assert!(err("https://github.com/org/repo/tree/main#readme").contains("web page URL"));
    }

    #[test]
    fn a_repository_actually_named_tree_or_blob_is_not_a_browse_url() {
        assert_eq!(ok("https://host/org/tree"), "https://host/org/tree");
        assert_eq!(ok("https://host/org/blob.git"), "https://host/org/blob.git");
    }
}
