use crate::model::{CatalogProject, Solution};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const CURRENT_VERSION: u32 = 1;

/// In-memory hydration struct for the live store. Its ids are numeric counters
/// loaded from `SolutionsDb`; it is no longer written to disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolutionsConfig {
    pub version: u32,
    #[serde(default)]
    pub catalog: Vec<CatalogProject>,
    #[serde(default)]
    pub solutions: Vec<Solution>,
}

// `LoadError` and `load_or_default` are used by `migrate.rs` to parse
// the legacy `solutions.json` once into the SQLite DB. They can be
// deleted once the JSON file is no longer expected on any user's disk.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON: {0}")]
    Parse(#[from] serde_json::Error),
}

/// The legacy `solutions.json` shape (TEXT slugs). Only `migrate.rs` parses it;
/// the live config uses `SolutionsConfig`, whose ids are counters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LegacySolutionsConfig {
    #[serde(default)]
    pub catalog: Vec<LegacyCatalogProject>,
    #[serde(default)]
    pub solutions: Vec<LegacySolution>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyCatalogProject {
    pub id: String,
    pub name: String,
    pub remote_url: String,
    #[serde(default)]
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacySolution {
    #[allow(dead_code)]
    pub id: String,
    pub name: String,
    pub root: std::path::PathBuf,
    #[serde(default)]
    pub members: Vec<LegacySolutionMember>,
    #[serde(default)]
    pub last_opened_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacySolutionMember {
    pub catalog_id: String,
    pub local_path: std::path::PathBuf,
}

pub fn load_or_default(path: &Path) -> Result<LegacySolutionsConfig, LoadError> {
    if !path.exists() {
        return Ok(LegacySolutionsConfig::default());
    }
    let raw = std::fs::read_to_string(path)?;
    let cfg: LegacySolutionsConfig = serde_json::from_str(&raw)?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn sample_config() -> SolutionsConfig {
        SolutionsConfig {
            version: CURRENT_VERSION,
            catalog: vec![CatalogProject {
                id: CatalogId(1),
                name: "ECOS Base".into(),
                remote_url: "git@example.com:ecos/ecos-base.git".into(),
                default_branch: Some("master".into()),
            }],
            solutions: vec![Solution {
                id: SolutionId(1),
                name: "ECOS Platform".into(),
                root: PathBuf::from("/home/user/sawe/solutions/ecos-platform"),
                members: vec![SolutionMember {
                    id: MemberId(1),
                    name: "ecos-base".into(),
                    local_path: PathBuf::from(
                        "/home/user/sawe/solutions/ecos-platform/ecos-base",
                    ),
                    origin_catalog_id: Some(CatalogId(1)),
                }],
                last_opened_at: None,
            }],
        }
    }

    #[test]
    fn round_trips_through_json() {
        let cfg = sample_config();
        let json = serde_json::to_string_pretty(&cfg).expect("to_string_pretty");
        let back: SolutionsConfig = serde_json::from_str(&json).expect("from_str");
        assert_eq!(cfg, back);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.json");
        let cfg = load_or_default(&path).expect("default");
        assert!(cfg.catalog.is_empty());
        assert!(cfg.solutions.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_error_does_not_overwrite() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("solutions.json");
        std::fs::write(&path, "not valid json {").expect("write corrupt");
        let result = load_or_default(&path);
        assert!(result.is_err());
        let preserved = std::fs::read_to_string(&path).expect("read after");
        assert_eq!(preserved, "not valid json {");
    }

    /// The legacy loader parses the OLD (slug-id) file shape — the only shape
    /// that can be on a user's disk. Parsing it with the numeric `SolutionsConfig`
    /// would fail on `"id": "cat-a"`.
    #[test]
    fn legacy_loader_parses_the_slug_shaped_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("solutions.json");
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "catalog": [
                    {"id": "cat-a", "name": "Cat A", "remote_url": "git@x:a"}
                ],
                "solutions": [
                    {
                        "id": "sol-1",
                        "name": "Sol One",
                        "root": "/tmp/sol-1",
                        "members": [
                            {"catalog_id": "cat-a", "local_path": "/tmp/sol-1/cat-a"}
                        ]
                    }
                ]
            }"#,
        )
        .expect("write");
        let cfg = load_or_default(&path).expect("parse legacy");
        assert_eq!(cfg.catalog.len(), 1);
        assert_eq!(cfg.catalog[0].id, "cat-a");
        assert_eq!(cfg.solutions.len(), 1);
        assert_eq!(cfg.solutions[0].members[0].catalog_id, "cat-a");
    }
}
