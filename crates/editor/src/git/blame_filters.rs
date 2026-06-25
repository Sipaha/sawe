//! S-ANN — author filter state for the blame gutter.
//!
//! Drives the "Filter Authors" dropdown in the annotate toolbar. The
//! state carries a normalized set of email addresses; entries whose
//! author email is **not** in the set are still rendered, just muted to
//! a single dot so the user can scan the file structure without losing
//! signal on the lines that matter to them.
//!
//! Constructing the filter empty (`AuthorFilter::default()`) means "show
//! all" — toggling the first author into the set switches into
//! whitelist mode.

use collections::{HashMap, HashSet};
use git::blame::BlameEntry;

use crate::git::blame_colors::normalize_email;

#[derive(Clone, Debug, Default)]
pub struct AuthorFilter {
    /// Emails (lowercased, brackets stripped) that should render the
    /// full annotation. When empty, all authors render fully.
    selected: HashSet<String>,
}

impl AuthorFilter {
    pub fn is_active(&self) -> bool {
        !self.selected.is_empty()
    }

    pub fn clear(&mut self) {
        self.selected.clear();
    }

    pub fn toggle(&mut self, email: &str) {
        let key = normalize_email(email);
        if !self.selected.remove(&key) {
            self.selected.insert(key);
        }
    }

    pub fn contains(&self, email: &str) -> bool {
        self.selected.contains(&normalize_email(email))
    }

    pub fn matches(&self, entry: &BlameEntry) -> bool {
        if !self.is_active() {
            return true;
        }
        match entry.author_mail.as_deref() {
            Some(email) => self.contains(email),
            None => false,
        }
    }
}

/// Sort key for the filter dropdown — by commit count desc, then author
/// name asc. Keeps the most prolific authors at the top so the user
/// rarely needs to scroll.
#[derive(Clone, Debug)]
pub struct AuthorStats {
    pub email: String,
    pub display_name: String,
    pub count: usize,
}

pub fn collect_author_stats(entries: &[BlameEntry]) -> Vec<AuthorStats> {
    let mut by_email: HashMap<String, AuthorStats> = HashMap::default();
    for entry in entries {
        let Some(email) = entry.author_mail.as_deref() else {
            continue;
        };
        let key = normalize_email(email);
        let display = entry.author.as_deref().unwrap_or(&key).to_string();
        let stats = by_email.entry(key.clone()).or_insert(AuthorStats {
            email: key,
            display_name: display,
            count: 0,
        });
        stats.count += 1;
    }
    let mut result: Vec<_> = by_email.into_values().collect();
    result.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::Oid;

    fn zero_oid() -> Oid {
        Oid::from_bytes(&[0; 20]).unwrap()
    }

    fn entry(email: &str, author: &str) -> BlameEntry {
        BlameEntry {
            sha: zero_oid(),
            range: 0..1,
            original_line_number: 1,
            author: Some(author.to_string()),
            author_mail: Some(email.to_string()),
            author_time: None,
            author_tz: None,
            committer_name: None,
            committer_email: None,
            committer_time: None,
            committer_tz: None,
            summary: None,
            previous: None,
            filename: String::new(),
        }
    }

    #[test]
    fn empty_filter_passes_everything() {
        let filter = AuthorFilter::default();
        assert!(filter.matches(&entry("<a@b.com>", "A")));
        assert!(!filter.is_active());
    }

    #[test]
    fn toggling_adds_then_removes_author() {
        let mut filter = AuthorFilter::default();
        filter.toggle("<alice@example.com>");
        assert!(filter.is_active());
        assert!(filter.matches(&entry("<Alice@Example.COM>", "Alice")));
        assert!(!filter.matches(&entry("<bob@example.com>", "Bob")));

        filter.toggle("<ALICE@example.com>");
        assert!(!filter.is_active());
    }

    #[test]
    fn collect_stats_sorts_by_count_desc() {
        let entries = vec![
            entry("<a@b.com>", "Alice"),
            entry("<a@b.com>", "Alice"),
            entry("<a@b.com>", "Alice"),
            entry("<c@d.com>", "Carol"),
            entry("<c@d.com>", "Carol"),
            entry("<e@f.com>", "Eve"),
        ];
        let stats = collect_author_stats(&entries);
        assert_eq!(stats.len(), 3);
        assert_eq!(stats[0].email, "a@b.com");
        assert_eq!(stats[0].count, 3);
        assert_eq!(stats[1].email, "c@d.com");
        assert_eq!(stats[1].count, 2);
        assert_eq!(stats[2].email, "e@f.com");
        assert_eq!(stats[2].count, 1);
    }

    #[test]
    fn entries_without_email_are_filtered_out() {
        let mut filter = AuthorFilter::default();
        filter.toggle("a@b.com");
        let entry_no_email = BlameEntry {
            sha: zero_oid(),
            range: 0..1,
            original_line_number: 1,
            author: Some("X".to_string()),
            author_mail: None,
            author_time: None,
            author_tz: None,
            committer_name: None,
            committer_email: None,
            committer_time: None,
            committer_tz: None,
            summary: None,
            previous: None,
            filename: String::new(),
        };
        assert!(!filter.matches(&entry_no_email));
    }
}
