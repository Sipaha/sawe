use settings::{RegisterSetting, Settings};
use std::time::Duration;

#[derive(Clone, Debug, RegisterSetting)]
pub struct SolutionAgentSettings {
    pub default_provider: gpui::SharedString,
    pub ephemeral: EphemeralPoolSettings,
    pub voice: VoiceSettings,
}

impl Default for SolutionAgentSettings {
    fn default() -> Self {
        Self {
            default_provider: crate::adapter::default_harness(),
            ephemeral: EphemeralPoolSettings::default(),
            voice: VoiceSettings::default(),
        }
    }
}

pub fn default_provider(cx: &gpui::App) -> gpui::SharedString {
    SolutionAgentSettings::try_get(cx)
        .map(|settings|if settings.default_provider.is_empty(){crate::adapter::default_harness()}else{settings.default_provider.clone()})
        .unwrap_or_else(|| crate::adapter::default_harness())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EphemeralPoolSettings {
    pub max_concurrent: u32,
    pub queue_timeout: Duration,
    pub idle_ttl: Duration,
}

impl Default for EphemeralPoolSettings {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            queue_timeout: Duration::from_secs(30),
            idle_ttl: Duration::from_secs(60),
        }
    }
}

/// Speech service configuration; contains an environment variable name, never a credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceSettings {
    pub api_url: String,
    pub api_key_env: String,
    pub transcription_url: Option<String>,
    pub speech_url: Option<String>,
    pub transcription_api_key_env: Option<String>,
    pub speech_api_key_env: Option<String>,
    pub transcription_model: String,
    pub speech_model: String,
    pub speech_voice: String,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            api_url: "http://127.0.0.1:8095/v1".into(),
            api_key_env: "SAWE_VOICE_API_KEY".into(),
            transcription_url: None,
            speech_url: None,
            transcription_api_key_env: None,
            speech_api_key_env: None,
            transcription_model: "gigaam-v3-ctc".into(),
            speech_model: "piper".into(),
            speech_voice: "ru_RU-denis-medium".into(),
        }
    }
}

impl Settings for SolutionAgentSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let defaults = EphemeralPoolSettings::default();
        let ephemeral = content
            .solution_agent
            .as_ref()
            .and_then(|s| s.ephemeral.as_ref())
            .map(|e| EphemeralPoolSettings {
                max_concurrent: e.max_concurrent.unwrap_or(defaults.max_concurrent),
                queue_timeout: e
                    .queue_timeout_seconds
                    .map(|s| Duration::from_secs(s as u64))
                    .unwrap_or(defaults.queue_timeout),
                idle_ttl: e
                    .idle_ttl_seconds
                    .map(|s| Duration::from_secs(s as u64))
                    .unwrap_or(defaults.idle_ttl),
            })
            .unwrap_or(defaults);
        let default_provider = content
            .solution_agent
            .as_ref()
            .and_then(|settings| settings.default_provider.as_ref())
            .map(|id| gpui::SharedString::from(id.clone()))
            .unwrap_or_else(|| crate::adapter::default_harness());
        let mut voice = VoiceSettings::default();
        if let Some(content) = content
            .solution_agent
            .as_ref()
            .and_then(|s| s.voice.as_ref())
        {
            if let Some(value) = &content.api_url {
                voice.api_url = value.clone();
            }
            if let Some(value) = &content.api_key_env {
                voice.api_key_env = value.clone();
            }
            voice.transcription_url = content.transcription_url.clone();
            voice.speech_url = content.speech_url.clone();
            voice.transcription_api_key_env = content.transcription_api_key_env.clone();
            voice.speech_api_key_env = content.speech_api_key_env.clone();
            if let Some(value) = &content.transcription_model {
                voice.transcription_model = value.clone();
            }
            if let Some(value) = &content.speech_model {
                voice.speech_model = value.clone();
            }
            if let Some(value) = &content.speech_voice {
                voice.speech_voice = value.clone();
            }
        }
        Self {
            default_provider,
            ephemeral,
            voice,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_defaults_are_opt_in_and_endpoint_overrides_survive_settings() {
        let defaults = SolutionAgentSettings::default().voice;
        assert_eq!(defaults.api_url, "http://127.0.0.1:8095/v1");
        assert_eq!(defaults.transcription_model, "gigaam-v3-ctc");
        let content: settings::SettingsContent = serde_json::from_value(serde_json::json!({
            "solution_agent": {"voice": {
                "transcription_url": "http://127.0.0.1:8080/inference",
                "speech_url": "http://127.0.0.1:8081/v1/audio/speech",
                "transcription_api_key_env": "LOCAL_STT_KEY",
                "speech_api_key_env": "LOCAL_TTS_KEY",
                "transcription_model": "small",
                "speech_voice": "local"
            }}
        }))
        .unwrap();
        let voice = SolutionAgentSettings::from_settings(&content).voice;
        assert_eq!(
            voice.transcription_url.as_deref(),
            Some("http://127.0.0.1:8080/inference")
        );
        assert_eq!(
            voice.speech_url.as_deref(),
            Some("http://127.0.0.1:8081/v1/audio/speech")
        );
        assert_eq!(
            voice.transcription_api_key_env.as_deref(),
            Some("LOCAL_STT_KEY")
        );
        assert_eq!(voice.speech_api_key_env.as_deref(), Some("LOCAL_TTS_KEY"));
        assert_eq!(voice.transcription_model, "small");
        assert_eq!(voice.speech_voice, "local");
        assert_eq!(voice.speech_model, defaults.speech_model);
    }

    #[test]
    fn defaults_match_plan() {
        let s = SolutionAgentSettings::default();
        assert_eq!(s.ephemeral.max_concurrent, 3);
        assert_eq!(s.ephemeral.queue_timeout, Duration::from_secs(30));
        assert_eq!(s.ephemeral.idle_ttl, Duration::from_secs(60));
    }
}
