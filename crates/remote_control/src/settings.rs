use anyhow::Result;

use crate::model::RemoteControlSettings;

pub(crate) fn parse(text: &str) -> Result<RemoteControlSettings> {
    if text.trim().is_empty() {
        return Ok(RemoteControlSettings::default());
    }
    let parsed = serde_json::from_str::<RemoteControlSettings>(text)?;
    Ok(parsed)
}

pub(crate) fn render(settings: &RemoteControlSettings) -> String {
    serde_json::to_string_pretty(settings)
        .map(|mut text| {
            text.push('\n');
            text
        })
        .unwrap_or_else(|_| "{}\n".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AuthorizedClient;
    use chrono::{DateTime, Utc};
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_empty_returns_default() {
        assert_eq!(parse("").expect("parse"), RemoteControlSettings::default());
        assert_eq!(
            parse("   \n").expect("parse"),
            RemoteControlSettings::default()
        );
    }

    #[test]
    fn parse_invalid_returns_error() {
        assert!(parse("not json").is_err());
    }

    #[test]
    fn render_round_trip() {
        let settings = RemoteControlSettings {
            server_address: Some("198.51.100.5".into()),
            server_port: 1234,
            enabled: false,
            clients: vec![AuthorizedClient {
                name: "Tablet".into(),
                secret_base64: "abcd".into(),
                created_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0)
                    .expect("valid timestamp"),
            }],
        };
        let text = render(&settings);
        assert!(text.ends_with('\n'));
        let parsed = parse(&text).expect("round-trip");
        assert_eq!(parsed, settings);
    }
}
