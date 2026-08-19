//! Classification of `git push` failures from git's own stderr.
//!
//! `git push` reports a refusal on stderr and exits non-zero; there is no
//! machine-readable channel short of `--porcelain`, which we cannot switch
//! to without changing what the rest of the pipeline parses. So the shape
//! the UI needs — "can the user fix this by pulling?" — is derived from the
//! text. The classifier deliberately never *replaces* git's message: callers
//! show the verbatim stderr and use the classification only to decide which
//! remediation buttons make sense.

/// Why the remote refused the push, as far as we can tell from stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushRejection {
    /// The remote branch has commits the local branch does not contain.
    /// git prints `(non-fast-forward)` or `(fetch first)`.
    NonFastForward,
    /// A `--force-with-lease` push whose lease no longer matches the remote
    /// ref: someone pushed after our last fetch. git prints `(stale info)`.
    StaleInfo,
    /// A server-side hook or branch protection rule refused the update.
    /// Pulling will not help here.
    HookDeclined,
    /// Credentials were missing, wrong, or insufficient.
    AuthenticationFailed,
    /// Anything we do not recognise. The verbatim message is still shown.
    Unknown,
}

impl PushRejection {
    /// Classify a failed push from the combined git stderr (passing the
    /// whole `anyhow` message is fine — matching is substring-based).
    pub fn classify(stderr: &str) -> Self {
        let text = stderr.to_ascii_lowercase();

        // `stale info` must be tested before the generic non-fast-forward
        // patterns: a rejected lease also prints `! [rejected]`, and the
        // remediation differs (a plain retry of the same lease will fail
        // again until we fetch).
        if text.contains("stale info") {
            return Self::StaleInfo;
        }
        if text.contains("non-fast-forward")
            || text.contains("fetch first")
            || text.contains("updates were rejected because the remote contains work")
            || text
                .contains("updates were rejected because the tip of your current branch is behind")
        {
            return Self::NonFastForward;
        }
        // Hook / protection refusals are reported as `[remote rejected]`,
        // which is a different marker from the local `[rejected]` above.
        if text.contains("hook declined")
            || text.contains("protected branch")
            || text.contains("remote rejected")
        {
            return Self::HookDeclined;
        }
        if text.contains("authentication failed")
            || text.contains("permission denied")
            || text.contains("could not read username")
            || text.contains("could not read password")
            || text.contains("invalid username or password")
        {
            return Self::AuthenticationFailed;
        }
        Self::Unknown
    }

    /// Whether the local and remote branches have diverged, i.e. whether
    /// pulling (rebase or merge) or re-leasing a force push is a meaningful
    /// next step. `HookDeclined` / `AuthenticationFailed` deliberately
    /// return `false` — offering "Pull with rebase" there sends the user
    /// down a dead end.
    pub fn is_diverged(self) -> bool {
        matches!(self, Self::NonFastForward | Self::StaleInfo)
    }

    /// Short human-readable summary shown above git's verbatim output.
    pub fn headline(self) -> &'static str {
        match self {
            Self::NonFastForward => {
                "Push rejected: the remote branch has commits you don't have locally."
            }
            Self::StaleInfo => {
                "Push rejected: the remote moved since your last fetch, so the lease is stale."
            }
            Self::HookDeclined => "Push rejected by the remote (hook or branch protection).",
            Self::AuthenticationFailed => "Push failed: the remote rejected your credentials.",
            Self::Unknown => "Push failed.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_non_fast_forward() {
        let stderr = concat!(
            "To github.com:Sipaha/sawe.git\n",
            " ! [rejected]        main -> main (fetch first)\n",
            "error: failed to push some refs to 'github.com:Sipaha/sawe.git'\n",
            "hint: Updates were rejected because the remote contains work that you do\n",
            "hint: not have locally.\n",
        );
        assert_eq!(
            PushRejection::classify(stderr),
            PushRejection::NonFastForward
        );
        assert!(PushRejection::classify(stderr).is_diverged());

        let stderr = " ! [rejected]        main -> main (non-fast-forward)\n";
        assert_eq!(
            PushRejection::classify(stderr),
            PushRejection::NonFastForward
        );
    }

    #[test]
    fn stale_lease_wins_over_generic_rejection() {
        let stderr = concat!(
            "To github.com:Sipaha/sawe.git\n",
            " ! [rejected]        main -> main (stale info)\n",
            "error: failed to push some refs\n",
        );
        assert_eq!(PushRejection::classify(stderr), PushRejection::StaleInfo);
        assert!(PushRejection::classify(stderr).is_diverged());
    }

    #[test]
    fn classifies_hook_and_auth_as_not_diverged() {
        let hook = " ! [remote rejected] main -> main (pre-receive hook declined)\n";
        assert_eq!(PushRejection::classify(hook), PushRejection::HookDeclined);
        assert!(!PushRejection::classify(hook).is_diverged());

        let auth = "fatal: Authentication failed for 'https://github.com/Sipaha/sawe.git/'\n";
        assert_eq!(
            PushRejection::classify(auth),
            PushRejection::AuthenticationFailed
        );
        assert!(!PushRejection::classify(auth).is_diverged());
    }

    #[test]
    fn unknown_failures_stay_unknown() {
        let stderr = "fatal: unable to access 'https://example.com/': Could not resolve host\n";
        assert_eq!(PushRejection::classify(stderr), PushRejection::Unknown);
        assert!(!PushRejection::classify(stderr).is_diverged());
    }
}
