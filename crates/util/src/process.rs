use crate::orphan_registry::{self, Registration};
use anyhow::{Context as _, Result};
use std::process::Stdio;

/// A wrapper around `smol::process::Child` that ensures all subprocesses
/// are killed when the process is terminated by using process groups.
pub struct Child {
    process: smol::process::Child,
    /// Present only for [`Child::spawn_tracked`]. Dropping it un-records the
    /// pid, so the on-disk registry holds exactly the processes no live
    /// `Child` is accounting for — see [`crate::orphan_registry`].
    _registration: Option<Registration>,
}

impl std::ops::Deref for Child {
    type Target = smol::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.process
    }
}

impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process
    }
}

impl Child {
    #[cfg(not(windows))]
    pub fn spawn(
        mut command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        crate::set_pre_exec_to_start_new_session(&mut command);
        let mut command = smol::process::Command::from(command);
        let process = command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn command {}",
                    crate::redact::redact_command(&format!("{command:?}"))
                )
            })?;
        Ok(Self {
            process,
            _registration: None,
        })
    }

    #[cfg(windows)]
    pub fn spawn(
        command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        // TODO(windows): create a job object and add the child process handle to it,
        // see https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects
        let mut command = smol::process::Command::from(command);
        let process = command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn command {}",
                    crate::redact::redact_command(&format!("{command:?}"))
                )
            })?;

        Ok(Self {
            process,
            _registration: None,
        })
    }

    /// [`Child::spawn`] plus a marker in the on-disk orphan registry, so a run
    /// that CRASHES does not leave this process running: the next startup's
    /// sweep finds the marker and kills the group.
    ///
    /// For long-lived subprocesses only — agent backends, whose orphans keep
    /// editing the worktree the next run reopens. A short-lived helper would
    /// pay two file operations for a window too small to matter.
    ///
    /// `label` is recorded alongside the pid and shows up in the reaping log
    /// line; it must not carry secrets, since the registry is a plain file.
    pub fn spawn_tracked(
        command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
        label: &str,
    ) -> Result<Self> {
        let mut child = Self::spawn(command, stdin, stdout, stderr)?;
        child._registration = orphan_registry::register(child.process.id(), label);
        Ok(child)
    }

    /// Hands out the raw child, giving up the orphan-registry marker with it:
    /// the registration is dropped here while the process keeps running. Only
    /// callers that used the untracked [`Child::spawn`] should use this.
    pub fn into_inner(self) -> smol::process::Child {
        self.process
    }

    #[cfg(not(windows))]
    pub fn kill(&mut self) -> Result<()> {
        let pid = self.process.id();
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn kill(&mut self) -> Result<()> {
        // TODO(windows): terminate the job object in kill
        self.process.kill()?;
        Ok(())
    }
}
