//! OpenAI-compatible speech transport, independent of the selected coding agent.
use crate::agent_settings::VoiceSettings;
use anyhow::{Result, anyhow, bail, ensure};
use futures::{AsyncReadExt, FutureExt};
use http_client::{AsyncBody, HttpClient, HttpRequestExt, RedirectPolicy, Request, Url};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use std::{sync::Arc, time::Duration};

pub const MAX_RECORDING_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_SPEECH_CHARACTERS: usize = 4000;
const MAX_TRANSCRIPT_BYTES: usize = 256 * 1024;
const MAX_SPEECH_BYTES: usize = 24 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

/// Does not implement Debug: the authorization header is private and must not be logged.
#[derive(Clone)]
pub struct VoiceService {
    settings: VoiceSettings,
    transcription_url: Url,
    speech_url: Url,
    transcription_authorization: Option<http_client::http::HeaderValue>,
    speech_authorization: Option<http_client::http::HeaderValue>,
    client: Arc<dyn HttpClient>,
}

impl VoiceService {
    pub fn new(settings: VoiceSettings, client: Arc<dyn HttpClient>) -> Result<Self> {
        Self::with_environment(settings, client, |name| std::env::var(name).ok())
    }

    fn with_environment(
        settings: VoiceSettings,
        client: Arc<dyn HttpClient>,
        environment: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let base_url = validate_url(&settings.api_url)?;
        ensure!(
            valid_env_name(&settings.api_key_env),
            "Voice API key environment variable name is invalid"
        );
        for value in [
            &settings.transcription_model,
            &settings.speech_model,
            &settings.speech_voice,
        ] {
            ensure!(
                !value.trim().is_empty()
                    && value.len() <= 256
                    && !value.chars().any(char::is_control),
                "Voice model and voice names must be nonempty and contain no control characters"
            );
        }
        let transcription_url = endpoint_url(
            &base_url,
            settings.transcription_url.as_deref(),
            "audio/transcriptions",
        )?;
        let speech_url = endpoint_url(&base_url, settings.speech_url.as_deref(), "audio/speech")?;
        let transcription_authorization = authorization(
            &settings,
            &base_url,
            &transcription_url,
            settings.transcription_api_key_env.as_deref(),
            &environment,
        )?;
        let speech_authorization = authorization(
            &settings,
            &base_url,
            &speech_url,
            settings.speech_api_key_env.as_deref(),
            &environment,
        )?;
        Ok(Self {
            settings,
            transcription_url,
            speech_url,
            transcription_authorization,
            speech_authorization,
            client,
        })
    }

    pub async fn transcribe(&self, wav: Vec<u8>) -> Result<String> {
        ensure!(
            wav.len() <= MAX_RECORDING_BYTES,
            "Voice recording exceeds the 20 MiB limit"
        );
        ensure!(is_wav(&wav), "Voice recording is not a WAV audio file");
        let boundary = format!("sawe-voice-{}", uuid::Uuid::new_v4());
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"recording.wav\"\r\nContent-Type: audio/wav\r\n\r\n",
            self.settings.transcription_model,
        ).into_bytes();
        body.extend(wav);
        body.extend(format!("\r\n--{boundary}--\r\n").as_bytes());
        let bytes = self
            .request(
                &self.transcription_url,
                self.transcription_authorization.as_ref(),
                &format!("multipart/form-data; boundary={boundary}"),
                body,
                MAX_TRANSCRIPT_BYTES,
            )
            .await?;
        #[derive(serde::Deserialize)]
        struct Transcript {
            text: String,
        }
        let transcript: Transcript = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("Voice transcription service returned malformed JSON"))?;
        let text = transcript.text.trim();
        ensure!(
            !text.is_empty(),
            "Voice transcription service returned an empty transcript"
        );
        Ok(text.to_owned())
    }

    pub async fn synthesize(&self, text: String) -> Result<Vec<u8>> {
        ensure!(!text.trim().is_empty(), "There is no text to read aloud");
        ensure!(
            text.chars().count() <= MAX_SPEECH_CHARACTERS,
            "Voice speech input exceeds the 4000 character limit"
        );
        let body = serde_json::to_vec(&serde_json::json!({
            "model": self.settings.speech_model,
            "input": text,
            "voice": self.settings.speech_voice,
            "response_format": "wav",
        }))?;
        let bytes = self
            .request(
                &self.speech_url,
                self.speech_authorization.as_ref(),
                "application/json",
                body,
                MAX_SPEECH_BYTES,
            )
            .await?;
        ensure!(
            is_wav(&bytes),
            "Voice speech service returned malformed or empty WAV audio"
        );
        Ok(bytes)
    }

    async fn request(
        &self,
        url: &Url,
        authorization: Option<&http_client::http::HeaderValue>,
        content_type: &str,
        body: Vec<u8>,
        limit: usize,
    ) -> Result<Vec<u8>> {
        ensure!(
            !is_official(url) || authorization.is_some(),
            "Voice requires a speech API key: set SAWE_VOICE_API_KEY (or configure solution_agent.voice.api_key_env), then restart Sawe"
        );
        let mut builder = Request::builder()
            .method("POST")
            .uri(url.as_str())
            .header("Content-Type", content_type)
            .follow_redirects(RedirectPolicy::NoFollow)
            .timeout(REQUEST_TIMEOUT);
        if let Some(authorization) = authorization {
            builder = builder.header("Authorization", authorization);
        }
        let request = builder
            .body(AsyncBody::from(body))
            .map_err(|_| anyhow!("Could not prepare voice service request"))?;
        let operation = async {
            // Discard transport errors and error bodies: either may contain credentials,
            // private transcript text, or an endpoint URL supplied by the service.
            let mut response = self.client.send(request).await.map_err(|_| {
                anyhow!("Could not connect to voice service; check its URL and network connection")
            })?;
            let status = response.status();
            if !status.is_success() {
                bail!(
                    "Voice service returned HTTP {}{}",
                    status.as_u16(),
                    match status.as_u16() {
                        401 | 403 => "; check speech service credentials",
                        429 => "; check speech service quota or retry later",
                        _ => "",
                    }
                );
            }
            let mut bytes = Vec::new();
            response
                .body_mut()
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| anyhow!("Could not read voice service response"))?;
            ensure!(
                bytes.len() <= limit,
                "Voice service response exceeds the size limit"
            );
            Ok(bytes)
        }
        .fuse();
        let timeout = smol::Timer::after(REQUEST_TIMEOUT).fuse();
        futures::pin_mut!(operation, timeout);
        futures::select! {
            result = operation => result,
            _ = timeout => Err(anyhow!("Voice service request timed out")),
        }
    }
}

fn endpoint_url(base: &Url, override_url: Option<&str>, path: &str) -> Result<Url> {
    validate_url(
        &override_url
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}/{path}", base.as_str().trim_end_matches('/'))),
    )
}

fn is_official(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("api.openai.com")
        && url.port_or_known_default() == Some(443)
}

fn authorization(
    settings: &VoiceSettings,
    base: &Url,
    endpoint: &Url,
    specific_env: Option<&str>,
    environment: &impl Fn(&str) -> Option<String>,
) -> Result<Option<http_client::http::HeaderValue>> {
    let key_env = match specific_env {
        Some(name) => {
            ensure!(
                valid_env_name(name),
                "Voice API key environment variable name is invalid"
            );
            name
        }
        None if endpoint.origin() == base.origin() => &settings.api_key_env,
        None => return Ok(None),
    };
    let credential = environment(key_env)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            (specific_env.is_none() && is_official(endpoint) && key_env == "SAWE_VOICE_API_KEY")
                .then(|| environment("OPENAI_API_KEY"))
                .flatten()
                .filter(|value| !value.trim().is_empty())
        });
    credential
        .map(|credential| {
            let mut header =
                http_client::http::HeaderValue::from_str(&format!("Bearer {credential}"))
                    .map_err(|_| anyhow!("Voice API key has an invalid format"))?;
            header.set_sensitive(true);
            Ok(header)
        })
        .transpose()
}

fn validate_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| anyhow!("Voice API URL is invalid"))?;
    let loopback = match url.host() {
        Some(http_client::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(http_client::Host::Ipv4(address)) => address.is_loopback(),
        Some(http_client::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "Voice API URL must use HTTPS, or HTTP on loopback"
    );
    ensure!(
        url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Voice API URL must have a host and contain no credentials, query or fragment"
    );
    Ok(url)
}

fn valid_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn is_wav(bytes: &[u8]) -> bool {
    if bytes.len() < 44 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"WAVE" {
        return false;
    }
    let mut position = 12usize;
    let mut has_format = false;
    while let Some(header) = bytes.get(position..position.saturating_add(8)) {
        let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        position += 8;
        let remaining = bytes.len() - position;
        if &header[..4] == b"data" {
            // Streaming WAV responses may use an unknown-length sentinel.
            return has_format
                && remaining > 0
                && (size == u32::MAX as usize || (size > 0 && size <= remaining));
        }
        if size > remaining {
            return false;
        }
        if &header[..4] == b"fmt " {
            if size < 16 {
                return false;
            }
            let format = &bytes[position..position + size];
            has_format = u16::from_le_bytes([format[2], format[3]]) > 0
                && u32::from_le_bytes(format[4..8].try_into().unwrap()) > 0
                && u16::from_le_bytes([format[12], format[13]]) > 0;
        }
        position = position.saturating_add(size).saturating_add(size % 2);
    }
    false
}

/// Prepare assistant prose for speech. Code blocks and images have no useful
/// spoken representation; link labels and inline code retain their readable text.
pub fn speech_text(markdown: &str) -> String {
    let mut output = String::new();
    let mut excluded = 0usize;
    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::CodeBlock(_) | Tag::Image { .. }) => excluded += 1,
            Event::End(TagEnd::CodeBlock | TagEnd::Image) => excluded = excluded.saturating_sub(1),
            _ if excluded > 0 => {}
            Event::Text(text) | Event::Code(text) => output.push_str(&text),
            Event::SoftBreak
            | Event::HardBreak
            | Event::Rule
            | Event::End(
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::TableCell,
            ) => output.push('\n'),
            _ => {}
        }
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{FakeHttpClient, Response};

    fn settings() -> VoiceSettings {
        VoiceSettings {
            api_url: "http://127.0.0.1:12345/v1".into(),
            ..Default::default()
        }
    }

    fn wav() -> Vec<u8> {
        let mut bytes = vec![0; 46];
        bytes[..4].copy_from_slice(b"RIFF");
        bytes[4..8].copy_from_slice(&38u32.to_le_bytes());
        bytes[8..16].copy_from_slice(b"WAVEfmt ");
        bytes[16..20].copy_from_slice(&16u32.to_le_bytes());
        bytes[20..22].copy_from_slice(&1u16.to_le_bytes());
        bytes[22..24].copy_from_slice(&1u16.to_le_bytes());
        bytes[24..28].copy_from_slice(&24000u32.to_le_bytes());
        bytes[28..32].copy_from_slice(&48000u32.to_le_bytes());
        bytes[32..34].copy_from_slice(&2u16.to_le_bytes());
        bytes[34..36].copy_from_slice(&16u16.to_le_bytes());
        bytes[36..40].copy_from_slice(b"data");
        bytes[40..44].copy_from_slice(&2u32.to_le_bytes());
        bytes
    }

    #[test]
    fn validates_endpoint_and_never_echoes_unsafe_url() {
        for url in [
            "http://example.com/v1",
            "https://secret@example.com/v1",
            "https://example.com/?key=secret",
            "https://example.com/#secret",
            "file:///secret",
        ] {
            let error = validate_url(url).unwrap_err().to_string();
            assert!(!error.contains("secret"));
        }
        for url in [
            "http://localhost:8000/v1",
            "http://127.0.0.2/v1",
            "http://[::1]:8000/v1",
            "https://example.com/v1",
        ] {
            assert!(validate_url(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn credentials_are_origin_scoped_and_sensitive() {
        let client = FakeHttpClient::with_200_response();
        let local = VoiceService::with_environment(settings(), client.clone(), |name| {
            assert_eq!(name, "SAWE_VOICE_API_KEY");
            None
        })
        .unwrap();
        assert!(local.transcription_authorization.is_none());
        let official =
            VoiceService::with_environment(VoiceSettings::default(), client.clone(), |name| {
                (name == "OPENAI_API_KEY").then(|| "secret".into())
            })
            .unwrap();
        assert!(official.transcription_authorization.unwrap().is_sensitive());
        let mut custom = settings();
        custom.api_url = "https://api.openai.com.example/v1".into();
        let service = VoiceService::with_environment(custom, client.clone(), |name| {
            assert_ne!(name, "OPENAI_API_KEY");
            None
        })
        .unwrap();
        assert!(service.transcription_authorization.is_none());
    }

    #[test]
    fn isolates_endpoint_overrides_and_allows_local_transcription_without_cloud_key() {
        smol::block_on(async {
            let mut config = VoiceSettings::default();
            config.transcription_url = Some("http://127.0.0.1:8080/inference".into());
            let client = FakeHttpClient::create(|request| async move {
                assert_eq!(request.uri().path(), "/inference");
                assert!(request.headers().get("Authorization").is_none());
                Ok(Response::new(r#"{"text":"Local transcript"}"#.into()))
            });
            let service =
                VoiceService::with_environment(config.clone(), client.clone(), |_| None).unwrap();
            assert_eq!(service.transcribe(wav()).await.unwrap(), "Local transcript");
            assert!(
                service
                    .synthesize("Hello".into())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("API key")
            );
            let service = VoiceService::with_environment(config.clone(), client.clone(), |_| {
                Some("cloud-key".into())
            })
            .unwrap();
            assert!(service.transcription_authorization.is_none());
            assert!(service.speech_authorization.is_some());
            config.transcription_api_key_env = Some("LOCAL_SPEECH_KEY".into());
            let service = VoiceService::with_environment(config, client, |name| {
                (name == "LOCAL_SPEECH_KEY").then(|| "local-key".into())
            })
            .unwrap();
            assert_eq!(
                service.transcription_authorization.unwrap(),
                "Bearer local-key"
            );
            assert!(service.speech_authorization.is_none());
        });
    }

    #[test]
    fn rejects_malformed_wav_and_accepts_streaming_length() {
        assert!(is_wav(&wav()));
        assert!(!is_wav(&wav()[..44]));
        let mut malformed = wav();
        malformed[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(!is_wav(&malformed));
        let mut streaming = wav();
        streaming[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(is_wav(&streaming));
    }

    #[test]
    fn transcribes_multipart_wav_with_request_guards() {
        smol::block_on(async {
            let client = FakeHttpClient::create(|mut request| async move {
                assert_eq!(request.uri().path(), "/v1/audio/transcriptions");
                assert_eq!(request.method(), "POST");
                assert!(request.headers().get("Authorization").is_none());
                assert_eq!(
                    request.extensions().get::<RedirectPolicy>(),
                    Some(&RedirectPolicy::NoFollow)
                );
                assert_eq!(
                    request
                        .extensions()
                        .get::<http_client::RequestTimeout>()
                        .unwrap()
                        .0,
                    REQUEST_TIMEOUT
                );
                assert!(
                    request.headers()["Content-Type"]
                        .to_str()
                        .unwrap()
                        .starts_with("multipart/form-data; boundary=")
                );
                let mut bytes = Vec::new();
                request.body_mut().read_to_end(&mut bytes).await.unwrap();
                let body = String::from_utf8_lossy(&bytes);
                assert!(body.contains("gpt-4o-mini-transcribe"));
                assert!(body.contains("filename=\"recording.wav\""));
                assert!(body.contains("RIFF"));
                Ok(Response::new(r#"{"text":"  Check tests  "}"#.into()))
            });
            let service = VoiceService::with_environment(settings(), client, |_| None).unwrap();
            assert_eq!(service.transcribe(wav()).await.unwrap(), "Check tests");
            assert!(service.transcribe(vec![]).await.is_err());
        });
    }

    #[test]
    fn synthesizes_wav_and_checks_json() {
        smol::block_on(async {
            let client = FakeHttpClient::create(|mut request| async move {
                assert_eq!(request.uri().path(), "/v1/audio/speech");
                let mut bytes = Vec::new();
                request.body_mut().read_to_end(&mut bytes).await.unwrap();
                let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(json["response_format"], "wav");
                assert_eq!(json["voice"], "coral");
                assert_eq!(json["input"], "Hello");
                Ok(Response::new(wav().into()))
            });
            let service = VoiceService::with_environment(settings(), client, |_| None).unwrap();
            assert_eq!(service.synthesize("Hello".into()).await.unwrap(), wav());
            assert!(service.synthesize(" ".into()).await.is_err());
            assert!(service.synthesize("я".repeat(4001)).await.is_err());
        });
    }

    #[test]
    fn rejects_bad_responses_without_disclosing_response_bodies() {
        smol::block_on(async {
            for status in [401, 403, 429, 500, 302] {
                let client = FakeHttpClient::create(move |_| async move {
                    Ok(Response::builder()
                        .status(status)
                        .body("secret-private-transcript".into())
                        .unwrap())
                });
                let service = VoiceService::with_environment(settings(), client, |_| None).unwrap();
                let error = service.transcribe(wav()).await.unwrap_err().to_string();
                assert!(error.contains(&status.to_string()));
                assert!(!error.contains("secret"));
            }
            for body in ["", "not-json", r#"{"text":"  "}"#, r#"{"other":"secret"}"#] {
                let client =
                    FakeHttpClient::create(move |_| async move { Ok(Response::new(body.into())) });
                let service = VoiceService::with_environment(settings(), client, |_| None).unwrap();
                assert!(service.transcribe(wav()).await.is_err());
                assert!(service.synthesize("Hello".into()).await.is_err());
            }
            let client =
                FakeHttpClient::create(|_| async { Err(anyhow!("secret transport failure")) });
            let service = VoiceService::with_environment(settings(), client, |_| None).unwrap();
            assert!(
                !service
                    .transcribe(wav())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("secret")
            );
        });
    }

    #[test]
    fn rejects_oversized_transcripts() {
        smol::block_on(async {
            let client = FakeHttpClient::create(|_| async {
                Ok(Response::new(vec![b' '; MAX_TRANSCRIPT_BYTES + 1].into()))
            });
            let service = VoiceService::with_environment(settings(), client, |_| None).unwrap();
            assert!(
                service
                    .transcribe(wav())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("size limit")
            );
        });
    }

    #[test]
    fn prepares_markdown_for_speech_without_code_or_image_noise() {
        assert_eq!(
            speech_text(
                "# Done\n\n**Fixed** [tests](https://example.com). Use `cargo test`.\n\n```rust\nsecret code\n```\n\n![image description](image.png)\n\nNext step."
            ),
            "Done Fixed tests. Use cargo test. Next step."
        );
        assert_eq!(speech_text("```\nonly code\n```"), "");
    }
}
