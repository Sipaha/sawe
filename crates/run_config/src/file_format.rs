use anyhow::{Context as _, Result};
use serde_json::{Map, Value};

use crate::model::{BeforeLaunchStep, ConfigScope, Executor, RunConfigId, RunConfiguration};

const RESERVED_KEYS: &[&str] = &["id", "name", "type", "executors", "before_launch", "folder"];

/// Parse a `run-configurations.json` document into `RunConfiguration`s.
///
/// `scope` is the scope to stamp on every entry (the caller knows whether this
/// file is the project file or the global file). Malformed individual entries
/// are skipped with a logged warning rather than failing the whole file.
pub fn parse_document(text: &str, scope: ConfigScope) -> Result<Vec<RunConfiguration>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let doc: Value =
        serde_json_lenient::from_str(text).context("parsing run-configurations.json")?;
    let entries = doc
        .get("configurations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::new();
    for entry in entries {
        match parse_entry(&entry, scope.clone()) {
            Ok(config) => out.push(config),
            Err(err) => log::warn!("skipping invalid run configuration entry: {err:#}"),
        }
    }
    Ok(out)
}

fn parse_entry(entry: &Value, scope: ConfigScope) -> Result<RunConfiguration> {
    let obj = entry.as_object().context("entry is not an object")?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .context("entry has no `name`")?
        .to_string();
    let provider_type = obj
        .get("type")
        .and_then(Value::as_str)
        .context("entry has no `type`")?
        .to_string();
    let executors: Vec<Executor> = obj
        .get("executors")
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .context("invalid `executors`")?
        .unwrap_or_else(|| vec![Executor::Run]);
    let before_launch: Vec<BeforeLaunchStep> = obj
        .get("before_launch")
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .context("invalid `before_launch`")?
        .unwrap_or_default();
    let folder = obj
        .get("folder")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    // A stable id, either read verbatim from the file or — for legacy entries
    // that predate the `"id"` key — derived deterministically from the name so
    // identity is stable for this load. `build_document` always writes `"id"`,
    // so the legacy id is materialized into the file on the next save.
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(RunConfigId::from_raw)
        .unwrap_or_else(|| legacy_id(&provider_type, &name));

    // Everything that isn't a reserved key is the provider payload.
    let settings: Map<String, Value> = obj
        .iter()
        .filter(|(k, _)| !RESERVED_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    Ok(RunConfiguration {
        id,
        name: name.into(),
        provider_type: provider_type.into(),
        settings: Value::Object(settings),
        executors,
        before_launch,
        folder: folder.map(Into::into),
        scope,
    })
}

/// Serialize a list of (persisted) configs back into a document `Value`. The
/// caller writes this with `serde_json::to_string_pretty`.
pub fn build_document(configs: &[RunConfiguration]) -> Value {
    let entries: Vec<Value> = configs
        .iter()
        .filter(|c| c.scope.is_persisted())
        .map(entry_value)
        .collect();
    serde_json::json!({ "configurations": entries })
}

/// The pre-`"id"` deterministic id: `"<provider_type>:<slugified name>"`. Kept
/// only for the legacy-fallback path in `parse_entry`.
fn legacy_id(provider_type: &str, name: &str) -> RunConfigId {
    RunConfigId::from_raw(format!("{provider_type}:{}", slugify(name)))
}

fn entry_value(config: &RunConfiguration) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), Value::String(config.id.as_str().to_string()));
    obj.insert("name".into(), Value::String(config.name.to_string()));
    obj.insert(
        "type".into(),
        Value::String(config.provider_type.to_string()),
    );
    obj.insert(
        "executors".into(),
        serde_json::to_value(&config.executors).unwrap_or(Value::Array(vec![])),
    );
    if !config.before_launch.is_empty() {
        obj.insert(
            "before_launch".into(),
            serde_json::to_value(&config.before_launch).unwrap_or(Value::Array(vec![])),
        );
    }
    if let Some(folder) = &config.folder {
        obj.insert("folder".into(), Value::String(folder.to_string()));
    }
    // Provider payload (preserves unknown keys carried in `settings`).
    if let Some(settings) = config.settings.as_object() {
        for (k, v) in settings {
            if !RESERVED_KEYS.contains(&k.as_str()) {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(obj)
}

pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() { "config".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn round_trips_and_preserves_unknown_keys() {
        let src = r#"
        {
          "configurations": [
            {
              "id": "the-id",
              "name": "Build release",
              "type": "shell",
              "executors": ["run"],
              "before_launch": ["save_all_files"],
              "folder": "Build",
              "command": "cargo",
              "args": ["build", "--release"],
              "future_field": { "x": 1 }
            }
          ]
        }
        "#;
        let configs = parse_document(src, ConfigScope::Global).unwrap();
        assert_eq!(configs.len(), 1);
        let c = &configs[0];
        assert_eq!(c.id.as_str(), "the-id");
        assert_eq!(c.name.as_ref(), "Build release");
        assert_eq!(c.provider_type.as_ref(), "shell");
        assert_eq!(c.executors, vec![Executor::Run]);
        assert_eq!(c.before_launch, vec![BeforeLaunchStep::SaveAllFiles]);
        assert_eq!(c.folder.as_ref().map(|s| s.as_ref()), Some("Build"));
        assert_eq!(c.settings["command"], serde_json::json!("cargo"));
        assert_eq!(c.settings["future_field"], serde_json::json!({ "x": 1 }));
        // `id` is reserved — not echoed into the provider payload.
        assert!(c.settings.get("id").is_none());

        let doc = build_document(&configs);
        let entry = &doc["configurations"][0];
        assert_eq!(entry["id"], serde_json::json!("the-id"));
        assert_eq!(entry["command"], serde_json::json!("cargo"));
        assert_eq!(entry["future_field"], serde_json::json!({ "x": 1 }));
        assert_eq!(
            entry["before_launch"],
            serde_json::json!(["save_all_files"])
        );
    }

    #[test]
    fn entry_without_id_gets_legacy_id_materialized_on_save() {
        let src = r#"
        {
          "configurations": [
            { "name": "Build release", "type": "shell", "command": "cargo" }
          ]
        }
        "#;
        let configs = parse_document(src, ConfigScope::Global).unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].id.as_str(), "shell:build-release");

        let doc = build_document(&configs);
        assert_eq!(
            doc["configurations"][0]["id"],
            serde_json::json!("shell:build-release")
        );
    }

    #[test]
    fn empty_and_missing_are_ok() {
        assert!(parse_document("", ConfigScope::Global).unwrap().is_empty());
        assert!(
            parse_document("{}", ConfigScope::Global)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn invalid_entries_are_skipped() {
        let src =
            r#"{ "configurations": [ { "type": "shell" }, { "name": "ok", "type": "shell" } ] }"#;
        let configs = parse_document(src, ConfigScope::Global).unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name.as_ref(), "ok");
    }

    #[test]
    fn slugify_handles_punctuation() {
        assert_eq!(slugify("Build release!"), "build-release");
        assert_eq!(slugify("  weird   name  "), "weird-name");
        assert_eq!(slugify("***"), "config");
    }
}
