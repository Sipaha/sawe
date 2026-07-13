use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Surrogate counter ids. They carry no meaning, are never derived from a name,
/// and never change — which is what makes rename cheap: the per-solution MCP
/// socket dir and every FK stay put across a rename.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CatalogId(pub i64);

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SolutionId(pub i64);

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemberId(pub i64);

impl std::fmt::Display for CatalogId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for SolutionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for MemberId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogProject {
    pub id: CatalogId,
    pub name: String,
    pub remote_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

/// A project inside a Solution. Independent of the catalog entry it was
/// instantiated from: `origin_catalog_id` records provenance and nothing
/// depends on it — editing or deleting the template never touches the member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolutionMember {
    pub id: MemberId,
    pub name: String,
    pub local_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_catalog_id: Option<CatalogId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Solution {
    pub id: SolutionId,
    pub name: String,
    pub root: PathBuf,
    #[serde(default)]
    pub members: Vec<SolutionMember>,
    /// Epoch millis. Was `DateTime<Utc>`; the DB column is INTEGER and every
    /// consumer that needs a formatted timestamp converts at its own edge with
    /// `chrono::DateTime::<chrono::Utc>::from_timestamp_millis`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_opened_at: Option<i64>,
}

impl Solution {
    pub fn first_member(&self) -> Option<&SolutionMember> {
        self.members.first()
    }

    pub fn member(&self, id: MemberId) -> Option<&SolutionMember> {
        self.members.iter().find(|m| m.id == id)
    }

    /// The member whose `local_path` is `path` or an ancestor of it. Used by the
    /// session/tab binding backfill to place a cwd inside a project.
    pub fn member_for_path(&self, path: &std::path::Path) -> Option<&SolutionMember> {
        self.members
            .iter()
            .filter(|m| path.starts_with(&m.local_path))
            .max_by_key(|m| m.local_path.as_os_str().len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solution_member_carries_local_path() {
        let m = SolutionMember {
            id: MemberId(7),
            name: "foo".into(),
            local_path: PathBuf::from("/tmp/foo"),
            origin_catalog_id: Some(CatalogId(3)),
        };
        assert_eq!(m.local_path, PathBuf::from("/tmp/foo"));
        assert_eq!(m.id, MemberId(7));
    }

    #[test]
    fn solution_first_member_returns_none_when_empty() {
        let s = Solution {
            id: SolutionId(1),
            name: "A".into(),
            root: PathBuf::from("/x"),
            members: vec![],
            last_opened_at: None,
        };
        assert!(s.first_member().is_none());
        assert!(s.member_for_path(std::path::Path::new("/x/foo")).is_none());
    }

    #[test]
    fn member_for_path_picks_the_longest_matching_member() {
        let s = Solution {
            id: SolutionId(1),
            name: "A".into(),
            root: PathBuf::from("/x"),
            members: vec![
                SolutionMember {
                    id: MemberId(1),
                    name: "foo".into(),
                    local_path: "/x/foo".into(),
                    origin_catalog_id: None,
                },
                SolutionMember {
                    id: MemberId(2),
                    name: "foo-nested".into(),
                    local_path: "/x/foo/nested".into(),
                    origin_catalog_id: None,
                },
            ],
            last_opened_at: None,
        };
        assert_eq!(
            s.member_for_path(std::path::Path::new("/x/foo/nested/src/a.rs"))
                .map(|m| m.id),
            Some(MemberId(2))
        );
        assert_eq!(
            s.member_for_path(std::path::Path::new("/x/foo/src/a.rs"))
                .map(|m| m.id),
            Some(MemberId(1))
        );
        assert_eq!(s.member_for_path(std::path::Path::new("/x")), None);
    }
}
