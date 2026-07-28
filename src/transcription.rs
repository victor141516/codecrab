use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, Response, StatusCode, multipart};
use serde::Deserialize;

use crate::{auth::OAuthStore, http_debug};

const CHATGPT_TRANSCRIBE_URL: &str = "https://chatgpt.com/backend-api/transcribe";

pub(crate) struct Transcriber {
    client: Client,
    auth: OAuthStore,
    debug_openai: bool,
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    text: String,
}

impl Transcriber {
    pub(crate) fn new(debug_openai: bool) -> Result<Self> {
        let mut auth = OAuthStore::new()?;
        auth.set_debug_openai(debug_openai);
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(90)).build()?,
            auth,
            debug_openai,
        })
    }

    pub(crate) async fn transcribe(&self, audio: Vec<u8>, content_type: &str) -> Result<String> {
        if audio.is_empty() {
            anyhow::bail!("recording is empty");
        }
        let credentials = self.auth.credentials().await?;
        let mut response = self.send(&credentials, audio.clone(), content_type).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            if self.debug_openai {
                log_response(response, true).await?;
            }
            let credentials = self.auth.refresh_credentials().await?;
            response = self.send(&credentials, audio, content_type).await?;
        }
        let status = response.status();
        let url = response.url().clone();
        let version = response.version();
        let headers = response.headers().clone();
        let body = response.text().await?;
        http_debug::response(self.debug_openai, &url, version, status, &headers, &body);
        if !status.is_success() {
            anyhow::bail!(
                "ChatGPT dictation returned {status}: {}",
                compact_error(&body)
            );
        }
        let transcript: TranscriptionResponse =
            serde_json::from_str(&body).context("ChatGPT returned an invalid transcription")?;
        let text = transcript.text.trim();
        if text.is_empty() {
            anyhow::bail!("ChatGPT returned an empty transcription");
        }
        Ok(text.to_owned())
    }

    async fn send(
        &self,
        credentials: &crate::auth::OAuthCredentials,
        audio: Vec<u8>,
        content_type: &str,
    ) -> Result<Response> {
        let extension = audio_extension(content_type);
        let part = multipart::Part::bytes(audio)
            .file_name(format!("codecrab.{extension}"))
            .mime_str(content_type)
            .context("invalid recording content type")?;
        let request = self
            .client
            .post(CHATGPT_TRANSCRIBE_URL)
            .bearer_auth(&credentials.access_token)
            .header("ChatGPT-Account-Id", &credentials.account_id)
            .header("originator", "codecrab")
            .multipart(multipart::Form::new().part("file", part))
            .build()
            .context("cannot build dictation request")?;
        http_debug::request(self.debug_openai, &request);
        self.client
            .execute(request)
            .await
            .context("ChatGPT dictation request failed")
    }
}

async fn log_response(response: Response, debug: bool) -> Result<()> {
    let status = response.status();
    let url = response.url().clone();
    let version = response.version();
    let headers = response.headers().clone();
    let body = response.text().await?;
    http_debug::response(debug, &url, version, status, &headers, &body);
    Ok(())
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

    #[test]
    fn recording_extensions_follow_the_mime_type() {
        assert_eq!(audio_extension("audio/wav"), "wav");
        assert_eq!(audio_extension("audio/webm;codecs=opus"), "webm");
        assert_eq!(audio_extension("audio/ogg"), "ogg");
    }

    #[tokio::test]
    #[ignore = "requires CODECRAB_TEST_AUDIO, network access, and a ChatGPT OAuth login"]
    async fn transcribes_audio_with_the_live_chatgpt_subscription() {
        let path = PathBuf::from(std::env::var_os("CODECRAB_TEST_AUDIO").unwrap());
        let audio = std::fs::read(path).unwrap();
        let transcript = Transcriber::new(false)
            .unwrap()
            .transcribe(audio, "audio/wav")
            .await
            .unwrap();
        assert!(!transcript.trim().is_empty());
    }
}
