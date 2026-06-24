use collections::HashMap;
use editor::Editor;
use gpui::{
    App, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled, Window,
};
use schemars::Schema;
use serde_json::Value;
use ui::{Checkbox, Label, LabelSize, ToggleState, prelude::*};

/// The kind of form field to render for a schema property.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldKind {
    Text,
    Number,
    Bool,
    StringArray,
    RawJson,
}

struct FieldRow {
    name: SharedString,
    kind: FieldKind,
    /// `None` only for `Bool` kind.
    editor: Option<Entity<Editor>>,
    bool_state: bool,
}

/// A GPUI view that renders an editable form from a top-level object schema.
pub struct SchemaForm {
    rows: Vec<FieldRow>,
}

impl SchemaForm {
    pub fn new(
        schema: &Schema,
        current: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let kinds = field_kinds(schema);
        let rows = kinds
            .into_iter()
            .map(|(name, kind)| {
                let current_value = current.get(&name);
                if kind == FieldKind::Bool {
                    FieldRow {
                        name: SharedString::from(name),
                        kind,
                        editor: None,
                        bool_state: initial_bool(current_value),
                    }
                } else {
                    let text = initial_text(&kind, current_value);
                    let is_multiline = matches!(kind, FieldKind::StringArray | FieldKind::RawJson);
                    let editor = cx.new(|cx| {
                        let mut editor = if is_multiline {
                            Editor::auto_height(2, 8, window, cx)
                        } else {
                            Editor::single_line(window, cx)
                        };
                        editor.set_text(text, window, cx);
                        editor
                    });
                    FieldRow {
                        name: SharedString::from(name),
                        kind,
                        editor: Some(editor),
                        bool_state: false,
                    }
                }
            })
            .collect();

        SchemaForm { rows }
    }

    /// Read the current edited state back as a JSON object.
    pub fn value(&self, cx: &App) -> Value {
        let mut texts: HashMap<String, String> = HashMap::default();
        let mut bools: HashMap<String, bool> = HashMap::default();

        for row in &self.rows {
            let name = row.name.to_string();
            if row.kind == FieldKind::Bool {
                bools.insert(name, row.bool_state);
            } else if let Some(editor) = &row.editor {
                texts.insert(name, editor.read(cx).text(cx));
            }
        }

        let kinds: Vec<(String, FieldKind)> = self
            .rows
            .iter()
            .map(|row| (row.name.to_string(), row.kind.clone()))
            .collect();

        assemble(&kinds, &texts, &bools)
    }

    /// Toggle a boolean checkbox row. Called from the checkbox's `on_click` handler.
    pub fn set_bool(&mut self, name: &str, value: bool, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.iter_mut().find(|row| row.name.as_ref() == name) {
            row.bool_state = value;
            cx.notify();
        }
    }
}

impl Render for SchemaForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut container = v_flex().gap_2();

        for row in &self.rows {
            let label = Label::new(row.name.clone()).size(LabelSize::Small);
            let name_for_closure = row.name.clone();

            let control: AnyElement = if row.kind == FieldKind::Bool {
                let is_selected = row.bool_state;
                Checkbox::new(
                    SharedString::from(format!("schema-form-{}", row.name)),
                    if is_selected {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .on_click(cx.listener(move |this, state: &ToggleState, _window, cx| {
                    this.set_bool(
                        name_for_closure.as_ref(),
                        *state == ToggleState::Selected,
                        cx,
                    );
                }))
                .into_any_element()
            } else if let Some(editor) = &row.editor {
                div().child(editor.clone()).into_any_element()
            } else {
                div().into_any_element()
            };

            container = container.child(v_flex().gap_1().child(label).child(control));
        }

        container
    }
}

// ---------------------------------------------------------------------------
// Pure functions (testable without GPUI)
// ---------------------------------------------------------------------------

/// Inspect the top-level object schema's `properties` and return `(property_name, kind)`
/// pairs. Returns an empty vec if the schema is not an object or has no properties.
pub fn field_kinds(schema: &Schema) -> Vec<(String, FieldKind)> {
    let properties = schema.get("properties").and_then(|value| value.as_object());

    let Some(properties) = properties else {
        return Vec::new();
    };

    properties
        .iter()
        .map(|(name, property_value)| {
            let kind = classify_property(property_value);
            (name.clone(), kind)
        })
        .collect()
}

/// Classify a single property's JSON schema value into a `FieldKind`.
fn classify_property(property: &Value) -> FieldKind {
    // `serde_json::Value` fields produce a `true` JSON boolean schema — no "type" key.
    // Unknown shapes fall back to RawJson.
    let type_value = match property.get("type") {
        Some(type_value) => type_value,
        None => return FieldKind::RawJson,
    };

    // "type" can be a string ("string") or an array (["string", "null"]).
    let types: Vec<&str> = match type_value {
        Value::String(single) => vec![single.as_str()],
        Value::Array(array) => array.iter().filter_map(|item| item.as_str()).collect(),
        _ => return FieldKind::RawJson,
    };

    if types.contains(&"boolean") {
        return FieldKind::Bool;
    }

    if types.contains(&"integer") || types.contains(&"number") {
        return FieldKind::Number;
    }

    if types.contains(&"string") {
        return FieldKind::Text;
    }

    if types.contains(&"array") {
        // Only treat as StringArray if items.type == "string".
        let is_string_array = property
            .get("items")
            .and_then(|items| items.get("type"))
            .and_then(|type_val| type_val.as_str())
            .map(|type_str| type_str == "string")
            .unwrap_or(false);
        if is_string_array {
            return FieldKind::StringArray;
        }
        return FieldKind::RawJson;
    }

    FieldKind::RawJson
}

/// Initial display text for a field given the current value (inverse of `assemble`).
pub fn initial_text(kind: &FieldKind, current: Option<&Value>) -> String {
    match kind {
        FieldKind::Text => current
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        FieldKind::Number => current.map(|value| value.to_string()).unwrap_or_default(),
        FieldKind::StringArray => current
            .and_then(|value| value.as_array())
            .map(|array| {
                array
                    .iter()
                    .filter_map(|item| item.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        FieldKind::RawJson => current
            .map(|value| serde_json::to_string_pretty(value).unwrap_or_default())
            .unwrap_or_default(),
        FieldKind::Bool => String::new(),
    }
}

/// Initial boolean state for a Bool field.
pub fn initial_bool(current: Option<&Value>) -> bool {
    current.and_then(|value| value.as_bool()).unwrap_or(false)
}

/// Reassemble a JSON object value from per-field string/bool inputs.
///
/// For `Number` fields: tries to parse as `i64`, then `f64`. Falls back to `null` on failure.
pub fn assemble(
    kinds: &[(String, FieldKind)],
    texts: &HashMap<String, String>,
    bools: &HashMap<String, bool>,
) -> Value {
    let mut map = serde_json::Map::new();

    for (name, kind) in kinds {
        let value = match kind {
            FieldKind::Text => {
                let text = texts.get(name).map(String::as_str).unwrap_or_default();
                Value::String(text.to_string())
            }
            FieldKind::Number => {
                let text = texts.get(name).map(String::as_str).unwrap_or_default();
                if let Ok(integer) = text.parse::<i64>() {
                    Value::Number(integer.into())
                } else if let Ok(float) = text.parse::<f64>() {
                    serde_json::Number::from_f64(float)
                        .map(Value::Number)
                        .unwrap_or(Value::Null)
                } else {
                    Value::Null
                }
            }
            FieldKind::Bool => {
                let checked = bools.get(name).copied().unwrap_or(false);
                Value::Bool(checked)
            }
            FieldKind::StringArray => {
                let text = texts.get(name).map(String::as_str).unwrap_or_default();
                let items: Vec<Value> = text
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| Value::String(line.trim().to_string()))
                    .collect();
                Value::Array(items)
            }
            FieldKind::RawJson => {
                let text = texts.get(name).map(String::as_str).unwrap_or_default();
                serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
            }
        };
        map.insert(name.clone(), value);
    }

    Value::Object(map)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, JsonSchema)]
    struct T {
        command: String,
        args: Vec<String>,
        count: i64,
        verbose: bool,
        extra: serde_json::Value,
    }

    #[test]
    fn detects_field_kinds() {
        let schema = schemars::schema_for!(T);
        let kinds = field_kinds(&schema);
        let by_name = |n: &str| {
            kinds
                .iter()
                .find(|(key, _)| key == n)
                .map(|(_, kind)| kind.clone())
        };
        assert_eq!(by_name("command"), Some(FieldKind::Text));
        assert_eq!(by_name("args"), Some(FieldKind::StringArray));
        assert_eq!(by_name("count"), Some(FieldKind::Number));
        assert_eq!(by_name("verbose"), Some(FieldKind::Bool));
        assert_eq!(by_name("extra"), Some(FieldKind::RawJson));
    }

    #[test]
    fn assembles_value() {
        let kinds = vec![
            ("command".to_string(), FieldKind::Text),
            ("args".to_string(), FieldKind::StringArray),
            ("verbose".to_string(), FieldKind::Bool),
        ];
        let mut texts = collections::HashMap::default();
        texts.insert("command".to_string(), "cargo".to_string());
        texts.insert("args".to_string(), "build\n--release\n".to_string());
        let mut bools = collections::HashMap::default();
        bools.insert("verbose".to_string(), true);
        let value = assemble(&kinds, &texts, &bools);
        assert_eq!(value["command"], serde_json::json!("cargo"));
        assert_eq!(value["args"], serde_json::json!(["build", "--release"]));
        assert_eq!(value["verbose"], serde_json::json!(true));
    }

    #[test]
    fn round_trips_via_initial_text() {
        let current = serde_json::json!({ "command": "echo", "args": ["a", "b"] });
        assert_eq!(
            initial_text(&FieldKind::Text, current.get("command")),
            "echo"
        );
        assert_eq!(
            initial_text(&FieldKind::StringArray, current.get("args")),
            "a\nb"
        );
    }
}
