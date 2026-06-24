use crate::model::{CatalogProject, Solution};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const CURRENT_VERSION: u32 = 1;

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
    #[error("unsupported config version {0} (newer than this build supports — upgrade Sawe)")]
    UnsupportedVersion(u32),
}

pub fn load_or_default(path: &Path) -> Result<SolutionsConfig, LoadError> {
    if !path.exists() {
        return Ok(SolutionsConfig {
            version: CURRENT_VERSION,
            ..Default::default()
        });
    }
    let raw = std::fs::read_to_string(path)?;
    let cfg: SolutionsConfig = serde_json::from_str(&raw)?;
    if cfg.version > CURRENT_VERSION {
        return Err(LoadError::UnsupportedVersion(cfg.version));
    }
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
                id: CatalogId("ecos-base".into()),
                name: "ECOS Base".into(),
                remote_url: "git@example.com:ecos/ecos-base.git".into(),
                default_branch: Some("master".into()),
            }],
            solutions: vec![Solution {
                id: SolutionId("ecos-platform".into()),
                name: "ECOS Platform".into(),
                root: PathBuf::from("/home/user/sawe/solutions/ecos-platform"),
                members: vec![SolutionMember {
                    catalog_id: CatalogId("ecos-base".into()),
                    local_path: PathBuf::from(
                        "/home/user/sawe/solutions/ecos-platform/ecos-base",
                    ),
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
        assert_eq!(cfg.version, CURRENT_VERSION);
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

    #[test]
    fn refuses_newer_version() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("solutions.json");
        std::fs::write(&path, r#"{"version":99,"catalog":[],"solutions":[]}"#).expect("write");
        let result = load_or_default(&path);
        assert!(matches!(result, Err(LoadError::UnsupportedVersion(99))));
    }
}
