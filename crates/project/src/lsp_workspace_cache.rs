//! A cache directory a language server can be pointed at, living inside the
//! Solution whose code it indexes.
//!
//! Some language servers keep a large on-disk index. The JetBrains
//! `kotlin-lsp` this was built for keeps 0.4–2.5 GB per project under
//! `~/.cache/JetBrains/IntelliJServer/workspaces/<opaque hash>`, where the hash
//! is computed from the exact set of workspace folders it was given. Three
//! things follow, and all three are bad:
//!
//! * nothing on disk says which project a directory belongs to — the only way
//!   to find out is to grep its serialised workspace model for absolute paths;
//! * adding or removing a member of a Solution changes the folder set, so the
//!   previous multi-gigabyte index is stranded, forever, unreferenced;
//! * deleting the project frees none of it.
//!
//! Pointing the server at `<solution>/.sawe/lsp/<server>/<generation>` fixes
//! all three at once: ownership is obvious from the path, the cache dies with
//! the Solution, and the generation key is ours, so the editor can collect the
//! stale ones itself instead of reverse-engineering a vendor's layout.
//!
//! The generation is keyed on the folder set for the same reason the server
//! keys on it — an index built for one set of roots is not an index for
//! another — but because WE compute it, a stale generation is a sibling
//! directory with an old `last-used` stamp rather than an opaque hash in a
//! shared cache.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// Marker rewritten every time a generation is handed to a server. Its mtime is
/// the whole freshness signal — collecting on it costs one `stat` per
/// generation, where inferring activity from the server's own files would mean
/// walking a multi-gigabyte tree.
const LAST_USED: &str = "last-used";

/// Human-readable record of what a generation was built for, so the answer to
/// "what is this 2 GB directory" is in the directory.
const ROOTS: &str = "roots.txt";

/// Generations handed out during this editor run.
///
/// The collector skips them unconditionally. `last-used` is stamped once at
/// server start, so a Solution left open for longer than the TTL would
/// otherwise have its LIVE index collected by a second server starting in the
/// same Solution. A process-wide set is the right lifetime for that: a
/// generation stops being live exactly when the editor that started its server
/// goes away.
static IN_USE: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// Prepare the cache directory for one server start, and collect the stale
/// generations beside it.
///
/// `roots` is the set of workspace folders the server is about to be given;
/// order does not matter. `ttl_days` of 0 disables collection.
///
/// Returns `None` if the directory cannot be created — the caller then starts
/// the server without the flag, which costs a shared cache rather than a failed
/// server. Does file I/O; call it off the main thread.
pub fn prepare(
    solution_root: &Path,
    server_name: &str,
    roots: &[PathBuf],
    ttl_days: u64,
) -> Option<PathBuf> {
    let server_dir = solution_root
        .join(".sawe")
        .join("lsp")
        .join(sanitize(server_name));
    let dir = server_dir.join(generation_key(roots));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        log::warn!("lsp workspace cache: cannot create {dir:?}: {error}");
        return None;
    }
    stamp(&dir, roots);
    mark_in_use(&dir);
    collect_stale(&server_dir, ttl_days);
    Some(dir)
}

/// Record that this generation is live, and refresh the freshness stamp.
fn stamp(dir: &Path, roots: &[PathBuf]) {
    if let Err(error) = std::fs::write(dir.join(LAST_USED), b"") {
        log::warn!("lsp workspace cache: cannot stamp {dir:?}: {error}");
    }
    let mut listed: Vec<String> = roots
        .iter()
        .map(|root| root.to_string_lossy().into_owned())
        .collect();
    listed.sort();
    if let Err(error) = std::fs::write(dir.join(ROOTS), listed.join("\n") + "\n") {
        log::warn!("lsp workspace cache: cannot record roots in {dir:?}: {error}");
    }
}

fn mark_in_use(dir: &Path) {
    match IN_USE.lock() {
        Ok(mut guard) => {
            guard
                .get_or_insert_with(HashSet::new)
                .insert(dir.to_path_buf());
        }
        Err(error) => log::warn!("lsp workspace cache: in-use set poisoned: {error}"),
    }
}

fn is_in_use(dir: &Path) -> bool {
    match IN_USE.lock() {
        Ok(guard) => guard.as_ref().is_some_and(|set| set.contains(dir)),
        // Fail closed: an unreadable set means "might be live", and keeping a
        // stale index costs disk where deleting a live one costs a re-index.
        Err(error) => {
            log::warn!("lsp workspace cache: in-use set poisoned: {error}");
            true
        }
    }
}

/// Delete generations under `server_dir` that no server has asked for in
/// `ttl_days`. Best effort: every failure is logged and stepped over.
fn collect_stale(server_dir: &Path, ttl_days: u64) {
    if ttl_days == 0 {
        return;
    }
    let ttl = Duration::from_secs(ttl_days * 24 * 60 * 60);
    let entries = match std::fs::read_dir(server_dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("lsp workspace cache: cannot scan {server_dir:?}: {error}");
            }
            return;
        }
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) || is_in_use(&dir) {
            continue;
        }
        // No stamp at all means a generation from before this mechanism, or one
        // whose creation was interrupted. Its age is unknown, so it is left
        // alone rather than guessed at; a `prepare` for the same folder set
        // will stamp it and bring it back under the TTL.
        let Ok(stamped) = std::fs::metadata(dir.join(LAST_USED)).and_then(|meta| meta.modified())
        else {
            continue;
        };
        let Ok(age) = now.duration_since(stamped) else {
            continue;
        };
        if age < ttl {
            continue;
        }
        log::info!(
            "lsp workspace cache: collecting {dir:?}, unused for {} days",
            age.as_secs() / 86_400
        );
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            log::warn!("lsp workspace cache: cannot remove {dir:?}: {error}");
        }
    }
}

/// A stable short name for one set of workspace folders.
///
/// FNV-1a rather than `DefaultHasher`: the key names a directory that has to
/// keep meaning the same thing across editor versions, and `DefaultHasher`'s
/// output is explicitly not guaranteed to be stable between releases.
fn generation_key(roots: &[PathBuf]) -> String {
    let mut sorted: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    sorted.sort();
    sorted.dedup();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for root in sorted {
        for byte in root.as_os_str().as_encoded_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0x1f;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Language server names are free-form and land in a path here.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A language server that keeps its index where we tell it to, and where that
/// is. Assembled on the App thread because it needs the Solution snapshot and
/// the assembled workspace folders; consumed in the background task that puts
/// the server's command line together. See [`crate::lsp_workspace_cache`].
pub struct WorkspaceCacheRequest {
    /// The server's own flag for "keep your state here", from
    /// `lsp.<name>.workspace_cache_flag`.
    pub flag: String,
    pub solution_root: PathBuf,
    pub server_name: String,
    /// The workspace folders this server is about to be given. The cache
    /// generation is keyed on them, because an index built for one set of roots
    /// is not an index for another.
    pub roots: Vec<PathBuf>,
    pub ttl_days: u64,
}

/// Resolve the request into a directory and append `<flag> <dir>` to the
/// command line. Silent no-op when there is no request, and a logged no-op when
/// the directory cannot be made: a server that falls back to its own default
/// cache still works, where one that fails to start does not.
pub fn append_argument(
    arguments: &mut Vec<std::ffi::OsString>,
    request: Option<WorkspaceCacheRequest>,
) {
    let Some(request) = request else { return };
    let Some(dir) = prepare(
        &request.solution_root,
        &request.server_name,
        &request.roots,
        request.ttl_days,
    ) else {
        return;
    };
    arguments.push(request.flag.into());
    arguments.push(dir.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn age_stamp(dir: &Path, days: u64) {
        let when = SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60 + 60);
        let file = std::fs::File::options()
            .write(true)
            .open(dir.join(LAST_USED))
            .expect("stamp exists");
        file.set_modified(when).expect("backdate stamp");
    }

    #[test]
    fn the_same_roots_get_the_same_generation_in_any_order() {
        let a = PathBuf::from("/ss/S/one");
        let b = PathBuf::from("/ss/S/two");
        assert_eq!(
            generation_key(&[a.clone(), b.clone()]),
            generation_key(&[b.clone(), a.clone()])
        );
        assert_ne!(
            generation_key(&[a.clone(), b.clone()]),
            generation_key(&[a.clone()]),
            "dropping a member is a different index"
        );
        // Concatenation must not collide: two roots that join into the same
        // byte string as one longer root are different folder sets.
        assert_ne!(
            generation_key(&[PathBuf::from("/a"), PathBuf::from("b")]),
            generation_key(&[PathBuf::from("/ab")])
        );
    }

    #[test]
    fn a_generation_records_what_it_was_built_for() {
        let solution = tempfile::tempdir().unwrap();
        let roots = vec![
            PathBuf::from("/ss/S/beta"),
            PathBuf::from("/ss/S/alpha"),
        ];

        let dir = prepare(solution.path(), "kotlin-lsp", &roots, 14).expect("prepared");

        assert!(dir.starts_with(solution.path().join(".sawe").join("lsp").join("kotlin-lsp")));
        let listed = std::fs::read_to_string(dir.join(ROOTS)).unwrap();
        assert_eq!(listed, "/ss/S/alpha\n/ss/S/beta\n", "sorted, one per line");
        assert!(dir.join(LAST_USED).exists());
    }

    #[test]
    fn a_stale_generation_is_collected_and_the_live_one_is_not() {
        let solution = tempfile::tempdir().unwrap();
        let old_roots = vec![PathBuf::from("/ss/S/alpha")];
        let new_roots = vec![PathBuf::from("/ss/S/alpha"), PathBuf::from("/ss/S/beta")];

        let old = prepare(solution.path(), "kotlin-lsp", &old_roots, 14).expect("prepared");
        std::fs::write(old.join("index.bin"), b"expensive").unwrap();
        // Pretend that generation was last used a month ago, and that the
        // editor which handed it out is gone.
        age_stamp(&old, 30);
        IN_USE.lock().unwrap().as_mut().unwrap().remove(&old);

        let new = prepare(solution.path(), "kotlin-lsp", &new_roots, 14).expect("prepared");

        assert!(new.exists());
        assert!(!old.exists(), "the stranded generation is collected");
    }

    #[test]
    fn a_stale_stamp_on_a_generation_still_in_use_is_ignored() {
        let solution = tempfile::tempdir().unwrap();
        let live_roots = vec![PathBuf::from("/ss/S/live")];
        let other_roots = vec![PathBuf::from("/ss/S/other")];

        let live = prepare(solution.path(), "kotlin-lsp", &live_roots, 14).expect("prepared");
        // A Solution left open longer than the TTL: the stamp is old, the
        // server is not.
        age_stamp(&live, 30);

        prepare(solution.path(), "kotlin-lsp", &other_roots, 14).expect("prepared");

        assert!(live.exists(), "a running server's index is never collected");
    }

    #[test]
    fn collection_can_be_switched_off() {
        let solution = tempfile::tempdir().unwrap();
        let old = prepare(solution.path(), "kotlin-lsp", &[PathBuf::from("/a")], 14).unwrap();
        age_stamp(&old, 365);
        IN_USE.lock().unwrap().as_mut().unwrap().remove(&old);

        prepare(solution.path(), "kotlin-lsp", &[PathBuf::from("/b")], 0).unwrap();

        assert!(old.exists(), "ttl 0 never collects");
    }

    #[test]
    fn a_generation_with_no_stamp_is_left_alone() {
        let solution = tempfile::tempdir().unwrap();
        let orphan = solution
            .path()
            .join(".sawe")
            .join("lsp")
            .join("kotlin-lsp")
            .join("written-by-an-older-build");
        std::fs::create_dir_all(&orphan).unwrap();

        prepare(solution.path(), "kotlin-lsp", &[PathBuf::from("/a")], 14).unwrap();

        assert!(orphan.exists(), "unknown age is not the same as old");
    }

    #[test]
    fn the_flag_and_directory_are_appended_to_the_command_line() {
        let solution = tempfile::tempdir().unwrap();
        let mut arguments: Vec<std::ffi::OsString> = vec!["--stdio".into()];

        append_argument(
            &mut arguments,
            Some(WorkspaceCacheRequest {
                flag: "--system-path".to_string(),
                solution_root: solution.path().to_path_buf(),
                server_name: "kotlin-lsp".to_string(),
                roots: vec![PathBuf::from("/ss/S/alpha")],
                ttl_days: 14,
            }),
        );

        assert_eq!(arguments.len(), 3, "the adapter's own arguments survive");
        assert_eq!(arguments[0], std::ffi::OsString::from("--stdio"));
        assert_eq!(arguments[1], std::ffi::OsString::from("--system-path"));
        let dir = PathBuf::from(&arguments[2]);
        assert!(
            dir.starts_with(solution.path().join(".sawe").join("lsp").join("kotlin-lsp")),
            "the index lands inside the Solution: {dir:?}"
        );
        assert!(dir.is_dir(), "the directory exists before the server starts");
    }

    #[test]
    fn a_server_outside_any_solution_keeps_its_own_cache() {
        let mut arguments: Vec<std::ffi::OsString> = vec!["--stdio".into()];

        append_argument(&mut arguments, None);

        assert_eq!(arguments, vec![std::ffi::OsString::from("--stdio")]);
    }

    #[test]
    fn a_server_name_cannot_escape_its_directory() {
        assert_eq!(sanitize("kotlin-lsp"), "kotlin-lsp");
        assert_eq!(sanitize("../../etc"), ".._.._etc");
        assert_eq!(sanitize("a b/c"), "a_b_c");
    }
}
