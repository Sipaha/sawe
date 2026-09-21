//! The Solution roots the editor currently knows about, kept as a process-wide
//! snapshot so `project` can answer "which Solution is this worktree part of"
//! without depending on the `solutions` crate.
//!
//! The dependency edge runs `solutions → project` (a Solution is a group of
//! worktrees, so it is built ON the project layer), and `project` cannot turn
//! it around. But a Solution-scoped language server — one server for every
//! member, decision #195 — needs a directory that belongs to the SOLUTION
//! rather than to whichever member happened to be picked as its anchor. That is
//! a fact only `solutions` owns and only `project` needs.
//!
//! So it travels the way `solutions::store`'s branch-protection snapshot
//! already travels: the owner writes it into a plain static in the lower crate,
//! and the consumer reads it. A gpui global would work too, but the read
//! happens while a language server is being assembled, and a lock-and-copy is
//! cheaper to reason about than an entity borrow on that path.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

static SOLUTION_ROOTS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Replace the known Solution roots. Called by `solutions` whenever its store
/// changes; the snapshot is whole-sale replaced rather than merged so a deleted
/// Solution disappears from it.
pub fn set_solution_roots(roots: Vec<PathBuf>) {
    match SOLUTION_ROOTS.lock() {
        Ok(mut guard) => *guard = roots,
        Err(error) => log::warn!("project::solution_roots: snapshot mutex poisoned: {error}"),
    }
}

/// The Solution `path` belongs to, if any.
///
/// The LONGEST matching root wins. Solutions are not nested today, but the
/// roots are user-configurable directories (`solutions.root`) and a shorter
/// prefix that merely contains another Solution would otherwise claim its
/// worktrees.
pub fn solution_root_for(path: &Path) -> Option<PathBuf> {
    let guard = SOLUTION_ROOTS
        .lock()
        .inspect_err(|error| {
            log::warn!("project::solution_roots: snapshot mutex poisoned: {error}")
        })
        .ok()?;
    guard
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.as_os_str().len())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The snapshot is process-wide, so these run as one test rather than
    // racing each other through a shared static.
    #[test]
    fn the_longest_matching_root_claims_a_path() {
        set_solution_roots(vec![
            PathBuf::from("/ss"),
            PathBuf::from("/ss/Inner"),
            PathBuf::from("/elsewhere"),
        ]);

        assert_eq!(
            solution_root_for(Path::new("/ss/Inner/member/src/main.kt")),
            Some(PathBuf::from("/ss/Inner")),
            "a nested Solution keeps its own worktrees"
        );
        assert_eq!(
            solution_root_for(Path::new("/ss/Other/member")),
            Some(PathBuf::from("/ss"))
        );
        assert_eq!(solution_root_for(Path::new("/tmp/scratch")), None);

        set_solution_roots(Vec::new());
        assert_eq!(
            solution_root_for(Path::new("/ss/Inner/member")),
            None,
            "a removed Solution stops claiming its worktrees"
        );
    }
}
