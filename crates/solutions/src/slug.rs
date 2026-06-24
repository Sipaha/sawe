use sha2::{Digest, Sha256};

pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_was_sep = true;
    for ch in input.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('-');
            last_was_sep = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        let hash = hasher.finalize();
        let prefix: u32 = hash[..4]
            .iter()
            .fold(0u32, |acc, byte| (acc << 8) | u32::from(*byte));
        out = format!("repo-{prefix:x}");
    }
    out
}

pub fn unique_slug(name: &str, taken: &[String]) -> String {
    let base = slugify(name);
    if !taken.iter().any(|t| t == &base) {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.iter().any(|t| t == &candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugifies_simple_name() {
        assert_eq!(slugify("ECOS Records"), "ecos-records");
    }

    #[test]
    fn collapses_runs_of_separators() {
        assert_eq!(slugify("foo  bar/baz"), "foo-bar-baz");
    }

    #[test]
    fn strips_leading_trailing_separators() {
        assert_eq!(slugify("--foo--"), "foo");
    }

    #[test]
    fn keeps_digits() {
        assert_eq!(slugify("ecos v2 module"), "ecos-v2-module");
    }

    #[test]
    fn falls_back_to_hash_for_empty_after_normalisation() {
        let s = slugify("漢字");
        assert!(
            !s.is_empty(),
            "got empty slug for non-ASCII-only input: {s:?}"
        );
        assert!(s.starts_with("repo-"), "expected hash fallback, got: {s:?}");
    }

    #[test]
    fn dedupes_against_existing() {
        let existing: Vec<String> = vec!["foo".into(), "foo-2".into()];
        assert_eq!(unique_slug("Foo", &existing), "foo-3");
    }

    #[test]
    fn dedupe_no_collision_returns_base() {
        let existing: Vec<String> = vec!["bar".into()];
        assert_eq!(unique_slug("Foo", &existing), "foo");
    }
}
