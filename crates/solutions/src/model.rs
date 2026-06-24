use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CatalogId(pub String);

impl CatalogId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SolutionId(pub String);

impl SolutionId {
    pub fn as_str(&self) -> &str {
        &self.0
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolutionMember {
    pub catalog_id: CatalogId,
    pub local_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Solution {
    pub id: SolutionId,
    pub name: String,
    pub root: PathBuf,
    #[serde(default)]
    pub members: Vec<SolutionMember>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_opened_at: Option<DateTime<Utc>>,
}

impl Solution {
    pub fn first_member(&self) -> Option<&SolutionMember> {
        self.members.first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_id_displays_inner() {
        let id = CatalogId("ecos-base".into());
        assert_eq!(id.as_str(), "ecos-base");
    }

    #[test]
    fn solution_member_carries_local_path() {
        let m = SolutionMember {
            catalog_id: CatalogId("foo".into()),
            local_path: PathBuf::from("/tmp/foo"),
        };
        assert_eq!(m.local_path, PathBuf::from("/tmp/foo"));
    }

    #[test]
    fn solution_first_member_returns_none_when_empty() {
        let s = Solution {
            id: SolutionId("a".into()),
            name: "A".into(),
            root: PathBuf::from("/x"),
            members: vec![],
            last_opened_at: None,
        };
        assert!(s.first_member().is_none());
    }

    #[test]
    fn solution_first_member_returns_first() {
        let s = Solution {
            id: SolutionId("a".into()),
            name: "A".into(),
            root: PathBuf::from("/x"),
            members: vec![
                SolutionMember {
                    catalog_id: CatalogId("foo".into()),
                    local_path: "/x/foo".into(),
                },
                SolutionMember {
                    catalog_id: CatalogId("bar".into()),
                    local_path: "/x/bar".into(),
                },
            ],
            last_opened_at: None,
        };
        assert_eq!(
            s.first_member().expect("non-empty").catalog_id.as_str(),
            "foo"
        );
    }
}
