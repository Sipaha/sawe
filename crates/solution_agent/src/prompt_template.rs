//! Render host-owned prompt placeholders without interpreting inserted values.

pub(crate) fn render(template: &str, replacements: &[(&str, &str)]) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some(start) = remaining.find('{') {
        rendered.push_str(&remaining[..start]);
        remaining = &remaining[start..];
        if let Some((key, value)) = replacements
            .iter()
            .find(|(key, _)| remaining.starts_with(*key))
        {
            rendered.push_str(value);
            remaining = &remaining[key.len()..];
        } else {
            rendered.push('{');
            remaining = &remaining[1..];
        }
    }
    rendered.push_str(remaining);
    rendered
}

/// JSON escaping alone does not quote an argument for the POSIX shell.
pub(crate) fn quote_shell_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn inserted_paths_and_user_instructions_are_not_reinterpreted() {
        assert_eq!(
            super::render(
                "{{path}} {custom}",
                &[
                    ("{{path}}", "/tmp/{custom}/it's"),
                    ("{custom}", "keep {{path}} literal")
                ]
            ),
            "/tmp/{custom}/it's keep {{path}} literal"
        );
    }
}
