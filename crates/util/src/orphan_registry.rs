//! On-disk registry of long-lived subprocesses, so a run that **crashes** does
//! not leave them running forever.
//!
//! An agent subprocess is deliberately `setsid`'d into its own process group
//! ([`crate::process::Child::spawn`]) so that `killpg` reaps the tools it
//! spawns. The price is that the OS does not take it down with the editor, and
//! the editor's own `on_app_quit` reaper only runs on a *clean* quit. Kill the
//! editor with `SIGKILL`, or let it crash, and every live `claude` keeps
//! executing its interrupted turn against the worktree the next run reopens the
//! same session in.
//!
//! So each tracked spawn drops a marker file here and removes it when the
//! process object goes away; the next startup sweeps whatever is left.
//!
//! ```text
//! <root>/
//!   <boot>.<owner pid>.<owner start>/      one directory per editor run
//!     <child pid>.<child start>            one file per live subprocess
//! ```
//!
//! Three facts make the sweep safe to run unattended:
//!
//! * **Pid reuse cannot cause a mis-kill.** A name carries the process start
//!   time (`/proc/<pid>/stat` field 22, in clock ticks since boot), and a
//!   recycled pid has a different one. Mismatch means "already gone".
//! * **A reboot cannot cause a mis-kill.** The owner directory carries the boot
//!   id, and start times are only comparable within one boot. Entries from an
//!   older boot are deleted without signalling anything.
//! * **A live run's children are never touched.** A directory whose owner is
//!   still alive is skipped whole, so two concurrent editors do not reap each
//!   other.
//!
//! Linux only. The identity check is the whole safety argument and it is built
//! on `/proc`; a platform where it cannot be made is better served by doing
//! nothing than by signalling pids on a guess. Everything below degrades to a
//! no-op elsewhere.

#[cfg(target_os = "linux")]
mod imp {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Where tracked spawns write their marker, and the identity of this run.
    /// Unset until [`init`] succeeds, which is what makes the registry inert in
    /// tests and in any process that never opted in.
    static ACTIVE: OnceLock<ActiveRun> = OnceLock::new();

    struct ActiveRun {
        /// `<root>/<boot>.<pid>.<start>` — created by `init`.
        dir: PathBuf,
    }

    /// Identity of an OS process that survives pid reuse: the pid plus the
    /// moment the kernel started it. Two processes can share a pid over time;
    /// they cannot share a pid *and* a start time.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct ProcessIdentity {
        pub pid: u32,
        /// Field 22 of `/proc/<pid>/stat`: clock ticks after boot at which the
        /// process started. Only meaningful within one boot, hence the boot id
        /// in the directory name.
        pub start_time: u64,
    }

    impl ProcessIdentity {
        /// The identity of a running process, or `None` if it is already gone.
        pub fn of(pid: u32) -> Option<Self> {
            let (start_time, _) = read_stat(pid)?;
            Some(Self { pid, start_time })
        }

        pub fn current() -> Option<Self> {
            Self::of(std::process::id())
        }

        /// Whether *this* process — not merely something wearing its pid — is
        /// still running.
        pub fn is_alive(&self) -> bool {
            ProcessIdentity::of(self.pid).as_ref() == Some(self)
        }

        pub(super) fn file_name(&self) -> String {
            format!("{}.{}", self.pid, self.start_time)
        }

        pub(super) fn parse_file_name(name: &str) -> Option<Self> {
            let (pid, start_time) = name.split_once('.')?;
            Some(Self {
                pid: pid.parse().ok()?,
                start_time: start_time.parse().ok()?,
            })
        }
    }

    /// `(start_time, process group id)` from `/proc/<pid>/stat`.
    ///
    /// The `comm` field is an arbitrary string wrapped in parentheses and may
    /// itself contain spaces and `)`, so the fields are counted from the LAST
    /// `)` — which is where every correct parser of this file starts. After it,
    /// token 0 is field 3 (`state`), so `pgrp` (field 5) is token 2 and
    /// `starttime` (field 22) is token 19.
    pub(super) fn read_stat(pid: u32) -> Option<(u64, u32)> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let tail = &stat[stat.rfind(')')? + 1..];
        let fields: Vec<&str> = tail.split_whitespace().collect();
        let pgrp = fields.get(2)?.parse().ok()?;
        let start_time = fields.get(19)?.parse().ok()?;
        Some((start_time, pgrp))
    }

    /// A stable id for the current boot. Start times are ticks since boot, so
    /// without this a pid+start pair recorded before a reboot could match an
    /// unrelated process after one.
    fn boot_id() -> Option<String> {
        let raw = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let id: String = raw
            .trim()
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .collect();
        (id.len() == 32).then_some(id)
    }

    /// What one sweep did. Returned rather than only logged so the behaviour is
    /// assertable — "killed nothing" and "found nothing" are different bugs.
    #[derive(Default, Debug, PartialEq, Eq)]
    pub struct ReapReport {
        /// Processes that were still running and got signalled.
        pub killed: usize,
        /// Recorded entries whose process had already exited (or whose pid had
        /// been recycled, which is indistinguishable from the outside and gets
        /// the same treatment: leave it alone).
        pub already_gone: usize,
        /// Previous-run directories removed.
        pub runs_swept: usize,
        /// Directories skipped because their run is still alive.
        pub runs_live: usize,
    }

    /// Start recording this run's tracked spawns under `root`.
    ///
    /// Call once, early in startup and after [`reap_orphans`], so this run's own
    /// directory is not a candidate for its own sweep. Failure is not fatal:
    /// the registry simply stays inert and crash reaping is lost for this run.
    pub fn init(root: &Path) {
        let Some(owner) = ProcessIdentity::current() else {
            log::warn!("orphan registry: cannot read own process identity; disabled");
            return;
        };
        let Some(boot) = boot_id() else {
            log::warn!("orphan registry: cannot read boot id; disabled");
            return;
        };
        let dir = root.join(format!("{boot}.{}", owner.file_name()));
        if let Err(error) = fs::create_dir_all(&dir) {
            log::warn!("orphan registry: cannot create {dir:?}: {error}; disabled");
            return;
        }
        let _ = ACTIVE.set(ActiveRun { dir });
    }

    /// Record `pid` until the returned guard is dropped.
    ///
    /// `None` when the registry is inert, which every caller treats as "no
    /// crash coverage", never as an error: losing the marker costs a stale
    /// process after a crash, while refusing to spawn would cost the feature.
    pub fn register(pid: u32, label: &str) -> Option<Registration> {
        let active = ACTIVE.get()?;
        register_in(&active.dir, pid, label)
    }

    /// [`register`], against an explicit directory. Separate so tests can drive
    /// the real thing without the process-wide `ACTIVE`.
    pub fn register_in(dir: &Path, pid: u32, label: &str) -> Option<Registration> {
        let identity = ProcessIdentity::of(pid)?;
        let path = dir.join(identity.file_name());
        let recorded_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();
        let body = serde_json::json!({ "label": label, "recorded_at": recorded_at });
        if let Err(error) = fs::write(&path, body.to_string()) {
            log::warn!("orphan registry: cannot record pid {pid} at {path:?}: {error}");
            return None;
        }
        Some(Registration { path })
    }

    /// Live marker for one tracked subprocess. Dropping it says "this process
    /// is accounted for" — which is true on every in-process path, because the
    /// owner of the guard is the owner of the process handle, and is exactly
    /// what a crash cannot do.
    pub struct Registration {
        path: PathBuf,
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_file(&self.path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("orphan registry: cannot remove {:?}: {error}", self.path);
            }
        }
    }

    /// Kill whatever previous runs left behind, and delete their directories.
    ///
    /// Best effort by construction: every failure here is logged and stepped
    /// over, because the alternative to an imperfect sweep is an editor that
    /// refuses to start.
    pub fn reap_orphans(root: &Path) -> ReapReport {
        let mut report = ReapReport::default();
        let Some(boot) = boot_id() else {
            log::warn!("orphan registry: cannot read boot id; skipping sweep");
            return report;
        };
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            // First ever run: nothing has been recorded yet.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return report,
            Err(error) => {
                log::warn!("orphan registry: cannot read {root:?}: {error}");
                return report;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            match parse_run_dir(name) {
                // A run from an earlier boot. Its pids mean nothing now and its
                // processes are long dead — delete, signal nothing.
                Some((run_boot, _)) if run_boot != boot => {
                    log::info!(
                        target: "util::orphan_registry",
                        "orphan registry: dropping pre-reboot run {name}"
                    );
                    remove_run_dir(&path);
                    report.runs_swept += 1;
                }
                Some((_, owner)) if owner.is_alive() => {
                    report.runs_live += 1;
                }
                Some((_, _)) => {
                    reap_run_dir(&path, &mut report);
                    remove_run_dir(&path);
                    report.runs_swept += 1;
                }
                // Not something we wrote. Leave it: this directory is ours by
                // convention only, and deleting unrecognised files is how a
                // cleanup routine turns into a data-loss bug.
                None => log::warn!("orphan registry: ignoring unrecognised entry {name}"),
            }
        }
        if report.killed > 0 {
            log::info!(
                target: "util::orphan_registry",
                "orphan registry: killed {} subprocess(es) left by a previous run",
                report.killed
            );
        }
        report
    }

    fn parse_run_dir(name: &str) -> Option<(String, ProcessIdentity)> {
        let (boot, owner) = name.split_once('.')?;
        if boot.len() != 32 || !boot.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some((boot.to_string(), ProcessIdentity::parse_file_name(owner)?))
    }

    fn reap_run_dir(dir: &Path, report: &mut ReapReport) {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                log::warn!("orphan registry: cannot read {dir:?}: {error}");
                return;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(recorded) = name.to_str().and_then(ProcessIdentity::parse_file_name) else {
                log::warn!("orphan registry: ignoring unrecognised entry {name:?}");
                continue;
            };
            if recorded.is_alive() {
                let label = read_label(&entry.path());
                log::info!(
                    target: "util::orphan_registry",
                    "orphan registry: killing pid {} left by a previous run ({label})",
                    recorded.pid
                );
                kill_orphan(recorded.pid);
                report.killed += 1;
            } else {
                report.already_gone += 1;
            }
            let _ = fs::remove_file(entry.path());
        }
    }

    /// The human-readable name a marker was recorded under, for the log line
    /// that says what is being killed. Best effort: a marker written by an
    /// older build, or truncated by a crash mid-write, still names a process
    /// worth reaping.
    fn read_label(path: &Path) -> String {
        fs::read_to_string(path)
            .ok()
            .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
            .and_then(|body| body.get("label")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unlabelled".to_string())
    }

    /// Kill the orphan and everything it started.
    ///
    /// A tracked spawn is a process-group leader (`setsid` at spawn), so
    /// `killpg` takes its whole tool subtree — an agent's builds and Bash calls
    /// are the reason the group exists. The leadership check is not a
    /// formality: `killpg` on a non-leader pid signals whatever group that pid
    /// happens to sit in, and for a process we did not `setsid` that group is
    /// *ours*. A lone `kill` is the safe degradation.
    fn kill_orphan(pid: u32) {
        match read_stat(pid) {
            Some((_, pgrp)) if pgrp == pid => unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            },
            Some((_, pgrp)) => {
                log::warn!(
                    "orphan registry: pid {pid} leads no process group (pgrp {pgrp}); \
                     killing it alone"
                );
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
            // Exited between the liveness check and here.
            None => {}
        }
    }

    fn remove_run_dir(dir: &Path) {
        if let Err(error) = fs::remove_dir_all(dir)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("orphan registry: cannot remove {dir:?}: {error}");
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::path::Path;

    #[derive(Default, Debug, PartialEq, Eq)]
    pub struct ReapReport {
        pub killed: usize,
        pub already_gone: usize,
        pub runs_swept: usize,
        pub runs_live: usize,
    }

    pub struct Registration;

    pub fn init(_root: &Path) {}

    pub fn register(_pid: u32, _label: &str) -> Option<Registration> {
        None
    }

    pub fn reap_orphans(_root: &Path) -> ReapReport {
        ReapReport::default()
    }
}

pub use imp::{ReapReport, Registration, init, reap_orphans, register};

#[cfg(target_os = "linux")]
pub use imp::{ProcessIdentity, register_in};

#[cfg(all(test, target_os = "linux"))]
use imp::read_stat as imp_read_stat_for_test;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{Child as StdChild, Command, Stdio};

    /// A `sleep` in its own process group, standing in for an agent backend.
    /// `setsid` is what the real spawn path does and what makes `killpg` on the
    /// recorded pid mean "this process and its tools".
    struct Sleeper {
        child: StdChild,
    }

    impl Sleeper {
        fn spawn() -> Self {
            let mut command = Command::new("sleep");
            command
                .arg("300")
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            crate::set_pre_exec_to_start_new_session(&mut command);
            Sleeper {
                child: command.spawn().expect("spawn sleep"),
            }
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }

        fn is_alive(&self) -> bool {
            // A killed child stays a zombie until reaped, and a zombie still
            // has a `/proc/<pid>/stat`, so liveness has to read the state
            // field rather than merely find the file.
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", self.pid())) else {
                return false;
            };
            let Some(tail) = stat.rfind(')').map(|at| &stat[at + 1..]) else {
                return false;
            };
            !matches!(tail.split_whitespace().next(), Some("Z") | None)
        }

        /// Poll rather than sleep: a `killpg` lands asynchronously and the only
        /// thing worth waiting for is the state change itself.
        fn wait_until_dead(&self, within: std::time::Duration) -> bool {
            let deadline = std::time::Instant::now() + within;
            while std::time::Instant::now() < deadline {
                if !self.is_alive() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            !self.is_alive()
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn boot() -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .collect()
    }

    /// A run directory named for an owner that cannot be alive: pid 0 is never
    /// a real process, so `ProcessIdentity::of` fails and the run reads as dead.
    fn dead_run_dir(root: &Path) -> PathBuf {
        let dir = root.join(format!("{}.0.1", boot()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn live_run_dir(root: &Path) -> PathBuf {
        let me = ProcessIdentity::current().unwrap();
        let dir = root.join(format!("{}.{}.{}", boot(), me.pid, me.start_time));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn identity_distinguishes_a_recycled_pid() {
        let sleeper = Sleeper::spawn();
        let identity = ProcessIdentity::of(sleeper.pid()).expect("live process has an identity");
        assert!(identity.is_alive());

        // Same pid, a start time that is not this process's: what a recycled
        // pid looks like from the registry's side.
        let impostor = ProcessIdentity {
            pid: identity.pid,
            start_time: identity.start_time + 1,
        };
        assert!(!impostor.is_alive());

        assert_eq!(ProcessIdentity::of(0), None, "pid 0 is never a process");
    }

    #[test]
    fn a_registration_records_the_pid_and_drops_it_again() {
        let root = tempfile::tempdir().unwrap();
        let dir = dead_run_dir(root.path());
        let sleeper = Sleeper::spawn();

        let registration = register_in(&dir, sleeper.pid(), "test agent").expect("registered");
        let recorded: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(recorded.len(), 1, "one marker per tracked process");
        let identity = ProcessIdentity::parse_file_name(&recorded[0]).expect("parsable name");
        assert_eq!(identity.pid, sleeper.pid());
        let body = std::fs::read_to_string(dir.join(&recorded[0])).unwrap();
        assert!(body.contains("test agent"), "label is recorded: {body}");

        drop(registration);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "a dropped registration leaves nothing to reap"
        );
    }

    #[test]
    fn a_dead_runs_live_subprocess_is_killed() {
        let root = tempfile::tempdir().unwrap();
        let dir = dead_run_dir(root.path());
        let sleeper = Sleeper::spawn();
        std::mem::forget(register_in(&dir, sleeper.pid(), "orphan").expect("registered"));

        let report = reap_orphans(root.path());

        assert_eq!(report.killed, 1);
        assert_eq!(report.runs_swept, 1);
        assert!(
            sleeper.wait_until_dead(std::time::Duration::from_secs(5)),
            "the orphan of a dead run is killed"
        );
        assert!(!dir.exists(), "the swept run directory is removed");
    }

    #[test]
    fn a_live_runs_subprocess_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let dir = live_run_dir(root.path());
        let sleeper = Sleeper::spawn();
        std::mem::forget(register_in(&dir, sleeper.pid(), "not yours").expect("registered"));

        let report = reap_orphans(root.path());

        assert_eq!(report.killed, 0);
        assert_eq!(report.runs_live, 1);
        assert_eq!(report.runs_swept, 0);
        assert!(
            sleeper.is_alive(),
            "another editor's agent survives the sweep"
        );
        assert!(dir.exists(), "a live run keeps its directory");
    }

    #[test]
    fn a_recycled_pid_is_not_killed() {
        let root = tempfile::tempdir().unwrap();
        let dir = dead_run_dir(root.path());
        let sleeper = Sleeper::spawn();
        let identity = ProcessIdentity::of(sleeper.pid()).unwrap();
        // The pid the previous run recorded now belongs to something else:
        // same number, different start time.
        std::fs::write(
            dir.join(format!("{}.{}", identity.pid, identity.start_time + 1)),
            "{}",
        )
        .unwrap();

        let report = reap_orphans(root.path());

        assert_eq!(report.killed, 0);
        assert_eq!(report.already_gone, 1);
        assert!(
            sleeper.is_alive(),
            "a process that merely inherited the pid is not killed"
        );
    }

    #[test]
    fn a_pre_reboot_run_is_deleted_without_signalling_anything() {
        let root = tempfile::tempdir().unwrap();
        let sleeper = Sleeper::spawn();
        let identity = ProcessIdentity::of(sleeper.pid()).unwrap();
        // Same pid and start time, recorded under a different boot: within that
        // boot this pid was some other process entirely.
        let dir = root.path().join(format!("{}.0.1", "f".repeat(32)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(identity.file_name()), "{}").unwrap();

        let report = reap_orphans(root.path());

        assert_eq!(report.killed, 0);
        assert_eq!(report.runs_swept, 1);
        assert!(sleeper.is_alive(), "start times do not cross a reboot");
        assert!(!dir.exists());
    }

    #[test]
    fn a_process_that_leads_no_group_is_killed_alone() {
        // A child spawned WITHOUT `setsid` sits in the test runner's own
        // process group. If the sweep reached for `killpg` here it would take
        // down this very process — so a green run is the assertion, and a
        // harness that dies mid-suite is the failure.
        let mut command = Command::new("sleep");
        command
            .arg("300")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("spawn sleep");
        let pid = child.id();
        let (_, pgrp) = super::imp_read_stat_for_test(pid).expect("live child");
        assert_ne!(pgrp, pid, "this child is deliberately not a group leader");

        let root = tempfile::tempdir().unwrap();
        let dir = dead_run_dir(root.path());
        std::mem::forget(register_in(&dir, pid, "not a leader").expect("registered"));

        let report = reap_orphans(root.path());

        assert_eq!(report.killed, 1);
        let status = child.wait().expect("wait");
        assert!(!status.success(), "the recorded process itself is killed");
        assert!(
            ProcessIdentity::current().unwrap().is_alive(),
            "the sweep did not signal its own process group"
        );
    }

    #[test]
    fn unrecognised_entries_are_left_where_they_are() {
        let root = tempfile::tempdir().unwrap();
        let stray = root.path().join("notes.txt");
        std::fs::write(&stray, "someone else's file").unwrap();

        let report = reap_orphans(root.path());

        assert_eq!(report, ReapReport::default());
        assert!(stray.exists(), "cleanup never deletes what it cannot parse");
    }

    #[test]
    fn a_missing_root_is_not_an_error() {
        let root = tempfile::tempdir().unwrap();
        let never_created = root.path().join("first-run");
        assert_eq!(reap_orphans(&never_created), ReapReport::default());
    }
}
