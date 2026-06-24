//! Branch tree builder for the Local / Remote tabs of the S-BRP popup.
//! Groups a flat list of branches into IDEA-style nested folders by `/`
//! prefix (`feature/foo`, `release/1.x`, `origin/feature/foo`, …).
//!
//! The tree is **flat-rendered** — every group becomes a header row in
//! the rows list, and child branches follow indented one level. This
//! keeps the existing uniform-list virtualization happy without forcing
//! a recursive renderer.

use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub enum TreeRow {
    /// Group separator at the given prefix path. `depth` is `0` for
    /// top-level groups, `1` for nested, etc. `expanded` controls
    /// whether children render below.
    Group {
        path: String,
        depth: usize,
        expanded: bool,
    },
    /// Branch leaf. `display_name` is the segment after the last `/`,
    /// e.g. `foo` for `feature/foo`. `full_name` is the original
    /// `Branch::name()`. `depth` matches its enclosing group depth `+1`.
    Leaf {
        full_name: String,
        display_name: String,
        depth: usize,
    },
}

#[derive(Debug, Default, Clone)]
pub struct BranchTree {
    pub rows: Vec<TreeRow>,
    /// Set of expanded group paths. Persisted with the popup so the
    /// user's choice survives tab switches.
    pub expanded: std::collections::HashSet<String>,
}

impl BranchTree {
    pub fn build(branches: &[String], expanded: std::collections::HashSet<String>) -> Self {
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut top_level: Vec<String> = Vec::new();
        for name in branches {
            match name.rsplit_once('/') {
                Some((prefix, _)) => {
                    groups
                        .entry(prefix.to_string())
                        .or_default()
                        .push(name.clone());
                }
                None => top_level.push(name.clone()),
            }
        }
        let mut rows = Vec::with_capacity(branches.len() + groups.len());
        for name in &top_level {
            rows.push(TreeRow::Leaf {
                full_name: name.clone(),
                display_name: name.clone(),
                depth: 0,
            });
        }
        for (prefix, mut children) in groups {
            let depth = prefix.matches('/').count();
            let is_expanded = expanded.contains(&prefix);
            rows.push(TreeRow::Group {
                path: prefix.clone(),
                depth,
                expanded: is_expanded,
            });
            if is_expanded {
                children.sort();
                for child in children {
                    let display = child
                        .rsplit_once('/')
                        .map(|(_, last)| last.to_string())
                        .unwrap_or_else(|| child.clone());
                    rows.push(TreeRow::Leaf {
                        full_name: child,
                        display_name: display,
                        depth: depth + 1,
                    });
                }
            }
        }
        Self { rows, expanded }
    }

    /// Toggle expansion of `path`. Returns the resulting `expanded`
    /// state for `path`.
    pub fn toggle(&mut self, path: &str) -> bool {
        if self.expanded.remove(path) {
            false
        } else {
            self.expanded.insert(path.to_string());
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn groups_branches_by_first_slash() {
        let branches = vec![
            "main".to_string(),
            "feature/login".to_string(),
            "feature/signup".to_string(),
            "release/1.0".to_string(),
        ];
        let mut expanded = HashSet::new();
        expanded.insert("feature".to_string());
        let tree = BranchTree::build(&branches, expanded);
        let group_paths: Vec<&str> = tree
            .rows
            .iter()
            .filter_map(|r| match r {
                TreeRow::Group { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert!(group_paths.contains(&"feature"));
        assert!(group_paths.contains(&"release"));
        // feature group is expanded so its children render in-tree.
        let leaves: Vec<&str> = tree
            .rows
            .iter()
            .filter_map(|r| match r {
                TreeRow::Leaf { full_name, .. } => Some(full_name.as_str()),
                _ => None,
            })
            .collect();
        assert!(leaves.contains(&"main"));
        assert!(leaves.contains(&"feature/login"));
        assert!(leaves.contains(&"feature/signup"));
        // release group is collapsed so its children are hidden.
        assert!(!leaves.contains(&"release/1.0"));
    }

    #[test]
    fn toggle_flips_expansion() {
        let mut tree = BranchTree::default();
        assert!(tree.toggle("feature"));
        assert!(tree.expanded.contains("feature"));
        assert!(!tree.toggle("feature"));
        assert!(!tree.expanded.contains("feature"));
    }
}
