use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, Response, StatusCode, multipart};
use serde::Deserialize;

use crate::{
    auth::OAuthStore,
    config::{Config, ProviderConfig},
    diagnostics::DebugOutput,
    http_debug,
};

const CHATGPT_TRANSCRIBE_URL: &str = "https://chatgpt.com/backend-api/transcribe";
const WHISPER_MODEL: &str = "whisper-1";

pub(crate) struct Transcriber {
    client: Client,
    backend: TranscriptionBackend,
    debug_openai: DebugOutput,
}

enum TranscriptionBackend {
    ChatGptSubscription {
        auth: OAuthStore,
    },
    ProviderApi {
        url: String,
        api_key: Option<String>,
    },
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    text: String,
}

impl Transcriber {
    pub(crate) fn new(
        config: &Config,
        provider_name: &str,
        debug_openai: impl Into<DebugOutput>,
    ) -> Result<Self> {
        let debug_openai = debug_openai.into();
        let provider = config.provider(provider_name)?;
        let auth_mode = provider.auth.trim().to_ascii_lowercase();
        let mut oauth = OAuthStore::new()?;
        oauth.set_debug_openai(debug_openai.clone());
        let use_oauth = match auth_mode.as_str() {
            "auto" => provider.is_official_openai() && oauth.is_logged_in(),
            "oauth" => {
                if !provider.is_official_openai() {
                    anyhow::bail!(
                        "ChatGPT OAuth can only be used with the official OpenAI provider"
                    );
                }
                if !oauth.is_logged_in() {
                    anyhow::bail!("not signed in with ChatGPT; run `codecrab auth login` first");
                }
                true
            }
            "api_key" | "none" => false,
            other => {
                anyhow::bail!("invalid auth mode {other:?}; expected auto, oauth, api_key, or none")
            }
        };
        let backend = if use_oauth {
            TranscriptionBackend::ChatGptSubscription { auth: oauth }
        } else {
            let api_key = match auth_mode.as_str() {
                "none" => None,
                "auto" | "api_key" if !provider.api_key.is_empty() => {
                    Some(provider.api_key.clone())
                }
                "auto" | "api_key" => {
                    anyhow::bail!("provider {provider_name:?} does not have an API key configured")
                }
                _ => unreachable!("OAuth was handled above"),
            };
            TranscriptionBackend::ProviderApi {
                url: provider_transcription_url(provider),
                api_key,
            }
        };
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(90)).build()?,
            backend,
            debug_openai,
        })
    }

    pub(crate) fn is_available(config: &Config, provider_name: &str) -> Result<bool> {
        if !config.provider(provider_name)?.is_official_openai() {
            return Ok(false);
        }
        Self::is_available_with_oauth(config, provider_name, OAuthStore::new()?.is_logged_in())
    }

    pub(crate) fn is_available_with_oauth(
        config: &Config,
        provider_name: &str,
        oauth_logged_in: bool,
    ) -> Result<bool> {
        let provider = config.provider(provider_name)?;
        if !provider.is_official_openai() {
            return Ok(false);
        }
        let auth_mode = provider.auth.trim().to_ascii_lowercase();
        match auth_mode.as_str() {
            "auto" => Ok(!provider.api_key.is_empty() || oauth_logged_in),
            "oauth" => Ok(oauth_logged_in),
            "api_key" => Ok(!provider.api_key.is_empty()),
            "none" => Ok(false),
            other => {
                anyhow::bail!("invalid auth mode {other:?}; expected auto, oauth, api_key, or none")
            }
        }
    }

    pub(crate) async fn transcribe(&self, audio: Vec<u8>, content_type: &str) -> Result<String> {
        if audio.is_empty() {
            anyhow::bail!("recording is empty");
        }
        let response = match &self.backend {
            TranscriptionBackend::ChatGptSubscription { auth } => {
                let credentials = auth.credentials().await?;
                let mut response = self
                    .send_subscription(&credentials, audio.clone(), content_type)
                    .await?;
                if response.status() == StatusCode::UNAUTHORIZED {
                    if self.debug_openai.is_enabled() {
                        log_response(response, &self.debug_openai).await?;
                    }
                    let credentials = auth.refresh_credentials().await?;
                    response = self
                        .send_subscription(&credentials, audio, content_type)
                        .await?;
                }
                response
            }
            TranscriptionBackend::ProviderApi { url, api_key } => {
                self.send_provider(url, api_key.as_deref(), audio, content_type)
                    .await?
            }
        };
        let status = response.status();
        let url = response.url().clone();
        let version = response.version();
        let headers = response.headers().clone();
        let body = response.text().await?;
        http_debug::response(&self.debug_openai, &url, version, status, &headers, &body)?;
        if !status.is_success() {
            anyhow::bail!(
                "voice transcription returned {status} from {url}: {}",
                compact_error(&body)
            );
        }
        let transcript: TranscriptionResponse =
            serde_json::from_str(&body).context("provider returned an invalid transcription")?;
        let text = transcript.text.trim();
        if text.is_empty() {
            anyhow::bail!("provider returned an empty transcription");
        }
        Ok(text.to_owned())
    }

    async fn send_subscription(
        &self,
        credentials: &crate::auth::OAuthCredentials,
        audio: Vec<u8>,
        content_type: &str,
    ) -> Result<Response> {
        let form = audio_form(audio, content_type)?;
        let request = self
            .client
            .post(CHATGPT_TRANSCRIBE_URL)
            .bearer_auth(&credentials.access_token)
            .header("ChatGPT-Account-Id", &credentials.account_id)
            .header("originator", "codecrab")
            .multipart(form)
            .build()
            .context("cannot build dictation request")?;
        self.execute(request, "ChatGPT dictation request").await
    }

    async fn send_provider(
        &self,
        url: &str,
        api_key: Option<&str>,
        audio: Vec<u8>,
        content_type: &str,
    ) -> Result<Response> {
        let form = audio_form(audio, content_type)?.text("model", WHISPER_MODEL);
        let mut request = self.client.post(url).multipart(form);
        if let Some(api_key) = api_key {
            request = request.bearer_auth(api_key);
        }
        let request = request
            .build()
            .context("cannot build provider transcription request")?;
        self.execute(request, "provider transcription request")
            .await
    }

    async fn execute(&self, request: reqwest::Request, context: &str) -> Result<Response> {
        http_debug::request(&self.debug_openai, &request)?;
        self.client
            .execute(request)
            .await
            .with_context(|| format!("{context} failed"))
    }
}

fn provider_transcription_url(provider: &ProviderConfig) -> String {
    format!(
        "{}/audio/transcriptions",
        provider.base_url.trim_end_matches('/')
    )
}

fn audio_form(audio: Vec<u8>, content_type: &str) -> Result<multipart::Form> {
    let extension = audio_extension(content_type);
    let part = multipart::Part::bytes(audio)
        .file_name(format!("codecrab.{extension}"))
        .mime_str(content_type)
        .context("invalid recording content type")?;
    Ok(multipart::Form::new().part("file", part))
}

async fn log_response(response: Response, debug: &DebugOutput) -> Result<()> {
    let status = response.status();
    let url = response.url().clone();
    let version = response.version();
    let headers = response.headers().clone();
    let body = response.text().await?;
    http_debug::response(debug, &url, version, status, &headers, &body)
}

fn audio_extension(content_type: &str) -> &'static str {
    let normalized = content_type.split(';').next().unwrap_or_default().trim();
    match normalized {
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/ogg" => "ogg",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        _ => "webm",
    }
}

fn compact_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("detail"))
                .or_else(|| value.get("message"))
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| body.chars().take(500).collect())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::DEFAULT_PROVIDER;

    #[test]
    fn recording_extensions_follow_the_mime_type() {
        assert_eq!(audio_extension("audio/wav"), "wav");
        assert_eq!(audio_extension("audio/webm;codecs=opus"), "webm");
        assert_eq!(audio_extension("audio/ogg"), "ogg");
    }

    #[test]
    fn compatible_provider_transcription_uses_its_own_base_url() {
        let mut config = Config::test("model", "https://provider.example/openai/v1/");
        let provider = config.providers.get_mut(DEFAULT_PROVIDER).unwrap();
        provider.auth = "api_key".into();
        provider.api_key = "provider-secret".into();

        let transcriber = Transcriber::new(&config, DEFAULT_PROVIDER, false).unwrap();
        let TranscriptionBackend::ProviderApi { url, api_key } = transcriber.backend else {
            panic!("a compatible provider must not use ChatGPT OAuth");
        };
        assert_eq!(
            url,
            "https://provider.example/openai/v1/audio/transcriptions"
        );
        assert_eq!(api_key.as_deref(), Some("provider-secret"));
        assert_eq!(
            reqwest::Url::parse(&url).unwrap().host_str(),
            Some("provider.example")
        );
    }

    #[test]
    fn compatible_provider_is_not_exposed_as_dictation_capable() {
        let config = Config::test("model", "https://provider.example/v1");
        assert!(!Transcriber::is_available_with_oauth(&config, DEFAULT_PROVIDER, true).unwrap());
    }

    #[test]
    fn official_openai_api_key_exposes_dictation() {
        let mut config = Config::default();
        let provider = config.providers.get_mut(DEFAULT_PROVIDER).unwrap();
        provider.auth = "api_key".into();
        provider.api_key = "sk-test".into();
        assert!(Transcriber::is_available_with_oauth(&config, DEFAULT_PROVIDER, false).unwrap());
    }

    #[tokio::test]
    #[ignore = "requires CODECRAB_TEST_AUDIO, network access, and a ChatGPT OAuth login"]
    async fn transcribes_audio_with_the_live_chatgpt_subscription() {
        let path = PathBuf::from(std::env::var_os("CODECRAB_TEST_AUDIO").unwrap());
        let audio = std::fs::read(path).unwrap();
        let config = Config::default();
        let transcript = Transcriber::new(&config, DEFAULT_PROVIDER, false)
            .unwrap()
            .transcribe(audio, "audio/wav")
            .await
            .unwrap();
        assert!(!transcript.trim().is_empty());
    }
}
