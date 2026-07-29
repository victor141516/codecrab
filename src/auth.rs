use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use url::Url;

use crate::{
    config::{ChatGptOAuthConfig, ConfigStore},
    http_debug,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const CALLBACK_PORTS: [u16; 2] = [1455, 1457];
const SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

pub(crate) struct OAuthCredentials {
    pub access_token: String,
    pub account_id: String,
}

pub(crate) struct AuthIdentity {
    pub email: Option<String>,
    pub plan: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

struct TokenClaims {
    account_id: Option<String>,
    email: Option<String>,
    plan: Option<String>,
}

pub(crate) struct OAuthStore {
    client: Client,
    debug_openai: bool,
    store: ConfigStore,
}

impl OAuthStore {
    pub(crate) fn new() -> Result<Self> {
        Self::with_store(ConfigStore::global()?)
    }

    fn with_store(store: ConfigStore) -> Result<Self> {
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(60)).build()?,
            debug_openai: false,
            store,
        })
    }

    pub(crate) fn set_debug_openai(&mut self, enabled: bool) {
        self.debug_openai = enabled;
    }

    pub(crate) fn is_logged_in(&self) -> bool {
        self.load().is_ok_and(|oauth| oauth.is_some())
    }

    pub(crate) fn status(&self) -> Result<Option<AuthIdentity>> {
        Ok(self.load()?.map(|oauth| AuthIdentity {
            email: oauth.email,
            plan: oauth.plan,
        }))
    }

    pub(crate) async fn login(&self) -> Result<AuthIdentity> {
        let (listener, port) = bind_callback().await?;
        let redirect_uri = format!("http://localhost:{port}/auth/callback");
        let verifier = random_urlsafe(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_urlsafe(32);
        let auth_url = authorize_url(&redirect_uri, &challenge, &state)?;

        println!("Opening your browser to sign in with ChatGPT…");
        println!("If it does not open, visit:\n{auth_url}\n");
        if let Err(error) = webbrowser::open(auth_url.as_str()) {
            eprintln!("Could not open the browser automatically: {error}");
        }

        let callback = timeout(Duration::from_secs(300), listener.accept())
            .await
            .context("login timed out after 5 minutes")??;
        let (code, mut socket) = read_callback(callback.0, &state).await?;
        let tokens = match self.exchange_code(&code, &redirect_uri, &verifier).await {
            Ok(tokens) => {
                respond_browser(
                    &mut socket,
                    200,
                    "CodeCrab is signed in. You can close this window.",
                )
                .await?;
                tokens
            }
            Err(error) => {
                let _ = respond_browser(
                    &mut socket,
                    500,
                    "CodeCrab could not complete sign-in. Return to the terminal.",
                )
                .await;
                return Err(error);
            }
        };
        self.save_token_response(tokens, None)
    }

    pub(crate) fn logout(&self) -> Result<()> {
        self.store.set_chatgpt_oauth(None)
    }

    pub(crate) async fn credentials(&self) -> Result<OAuthCredentials> {
        self.credentials_inner(false).await
    }

    pub(crate) async fn refresh_credentials(&self) -> Result<OAuthCredentials> {
        self.credentials_inner(true).await
    }

    async fn credentials_inner(&self, force_refresh: bool) -> Result<OAuthCredentials> {
        let access = self
            .load()?
            .context("not signed in with ChatGPT; run `codecrab auth login` first")?;
        if !force_refresh && access.expires_at > now_epoch() + 60 {
            return Ok(OAuthCredentials {
                access_token: access.access_token,
                account_id: access.account_id,
            });
        }

        let refresh = access.refresh_token.clone();
        let request = self
            .client
            .post(format!("{ISSUER}/oauth/token"))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.as_str()),
                ("client_id", CLIENT_ID),
            ])
            .build()
            .context("cannot build ChatGPT refresh request")?;
        http_debug::request(self.debug_openai, &request);
        let response = self
            .client
            .execute(request)
            .await
            .context("could not refresh ChatGPT login")?;
        let status = response.status();
        let version = response.version();
        let url = response.url().clone();
        let headers = response.headers().clone();
        let body = response.text().await?;
        http_debug::response(self.debug_openai, &url, version, status, &headers, &body);
        if !status.is_success() {
            anyhow::bail!(
                "ChatGPT login refresh failed ({status}); run `codecrab auth login` again"
            );
        }
        let tokens: TokenResponse =
            serde_json::from_str(&body).context("invalid token refresh response")?;
        self.save_token_response(tokens, Some(&access))?;
        let current = self
            .load()?
            .context("ChatGPT login disappeared from the global configuration")?;
        Ok(OAuthCredentials {
            access_token: current.access_token,
            account_id: current.account_id,
        })
    }

    async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<TokenResponse> {
        let request = self
            .client
            .post(format!("{ISSUER}/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", CLIENT_ID),
                ("code_verifier", verifier),
            ])
            .build()
            .context("cannot build ChatGPT token exchange request")?;
        http_debug::request(self.debug_openai, &request);
        let response = self
            .client
            .execute(request)
            .await
            .context("could not exchange the ChatGPT authorization code")?;
        let status = response.status();
        let version = response.version();
        let url = response.url().clone();
        let headers = response.headers().clone();
        let body = response.text().await?;
        http_debug::response(self.debug_openai, &url, version, status, &headers, &body);
        if !status.is_success() {
            anyhow::bail!("ChatGPT token exchange failed ({status})");
        }
        serde_json::from_str(&body).context("invalid ChatGPT token response")
    }

    fn save_token_response(
        &self,
        tokens: TokenResponse,
        previous: Option<&ChatGptOAuthConfig>,
    ) -> Result<AuthIdentity> {
        let claims = tokens
            .id_token
            .as_deref()
            .and_then(parse_claims)
            .or_else(|| parse_claims(&tokens.access_token));
        let account_id = claims
            .as_ref()
            .and_then(|claims| claims.account_id.clone())
            .or_else(|| previous.map(|oauth| oauth.account_id.clone()))
            .context("ChatGPT login did not include an account id")?;
        let oauth = ChatGptOAuthConfig {
            access_token: tokens.access_token,
            refresh_token: tokens
                .refresh_token
                .or_else(|| previous.map(|oauth| oauth.refresh_token.clone()))
                .context("ChatGPT login did not include a refresh token")?,
            expires_at: now_epoch() + tokens.expires_in,
            account_id,
            email: claims
                .as_ref()
                .and_then(|claims| claims.email.clone())
                .or_else(|| previous.and_then(|oauth| oauth.email.clone())),
            plan: claims
                .as_ref()
                .and_then(|claims| claims.plan.clone())
                .or_else(|| previous.and_then(|oauth| oauth.plan.clone())),
        };
        self.save(&oauth)?;
        Ok(AuthIdentity {
            email: oauth.email,
            plan: oauth.plan,
        })
    }

    fn save(&self, oauth: &ChatGptOAuthConfig) -> Result<()> {
        self.store.set_chatgpt_oauth(Some(oauth.clone()))
    }

    fn load(&self) -> Result<Option<ChatGptOAuthConfig>> {
        let oauth = self.store.load()?.chatgpt_oauth;
        if let Some(oauth) = &oauth {
            for (name, value) in [
                ("access_token", oauth.access_token.as_str()),
                ("refresh_token", oauth.refresh_token.as_str()),
                ("account_id", oauth.account_id.as_str()),
            ] {
                if value.is_empty() {
                    anyhow::bail!("chatgpt_oauth.{name} is empty in the global configuration");
                }
            }
        }
        Ok(oauth)
    }
}

async fn bind_callback() -> Result<(TcpListener, u16)> {
    for port in CALLBACK_PORTS {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            return Ok((listener, port));
        }
    }
    anyhow::bail!("OAuth callback ports 1455 and 1457 are both in use")
}

fn authorize_url(redirect_uri: &str, challenge: &str, state: &str) -> Result<Url> {
    let mut url = Url::parse(&format!("{ISSUER}/oauth/authorize"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "codecrab");
    Ok(url)
}

async fn read_callback(mut socket: TcpStream, expected_state: &str) -> Result<(String, TcpStream)> {
    let mut buffer = Vec::with_capacity(4096);
    loop {
        let mut chunk = [0; 2048];
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() >= 16_384 {
            anyhow::bail!("OAuth callback request is too large");
        }
    }
    let request = std::str::from_utf8(&buffer).context("invalid OAuth callback request")?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("invalid OAuth callback")?;
    let url = Url::parse(&format!("http://localhost{target}"))?;
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    if let Some(error) = params.get("error") {
        let description = params.get("error_description").unwrap_or(error);
        anyhow::bail!("ChatGPT sign-in was rejected: {description}");
    }
    if params.get("state").map(String::as_str) != Some(expected_state) {
        anyhow::bail!("OAuth state mismatch; possible cross-site request forgery");
    }
    let code = params
        .get("code")
        .filter(|code| !code.is_empty())
        .cloned()
        .context("OAuth callback did not include an authorization code")?;
    Ok((code, socket))
}

async fn respond_browser(socket: &mut TcpStream, status: u16, message: &str) -> Result<()> {
    let reason = if status == 200 {
        "OK"
    } else {
        "Internal Server Error"
    };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>CodeCrab</title><body style=\"font:18px system-ui;max-width:40rem;margin:15vh auto;padding:2rem\"><h1>🦀 CodeCrab</h1><p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await?;
    Ok(())
}

fn parse_claims(token: &str) -> Option<TokenClaims> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    let auth = value.get("https://api.openai.com/auth");
    Some(TokenClaims {
        account_id: value
            .get("chatgpt_account_id")
            .and_then(Value::as_str)
            .or_else(|| auth?.get("chatgpt_account_id")?.as_str())
            .map(str::to_owned),
        email: value
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_owned),
        plan: auth
            .and_then(|auth| auth.get("chatgpt_plan_type"))
            .and_then(Value::as_str)
            .map(normalize_plan),
    })
}

fn normalize_plan(plan: &str) -> String {
    match plan.to_ascii_lowercase().as_str() {
        "pro" | "prolite" => "Pro".into(),
        "plus" => "Plus".into(),
        "team" => "Team".into(),
        "business" => "Business".into(),
        "enterprise" => "Enterprise".into(),
        "free" => "Free".into(),
        _ => plan.to_owned(),
    }
}

fn random_urlsafe(bytes: usize) -> String {
    let mut value = vec![0; bytes];
    rand::rng().fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn default_expires_in() -> u64 {
    3600
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_uses_pkce_and_current_codex_scopes() {
        let url =
            authorize_url("http://localhost:1455/auth/callback", "challenge", "state").unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params.get("code_challenge").unwrap(), "challenge");
        assert_eq!(params.get("state").unwrap(), "state");
        assert!(
            params
                .get("scope")
                .unwrap()
                .contains("api.connectors.invoke")
        );
    }

    #[test]
    fn parses_nested_chatgpt_claims_without_exposing_token() {
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"email":"user@example.com","exp":9999999999,"https://api.openai.com/auth":{"chatgpt_account_id":"acct_123","chatgpt_plan_type":"pro"}}"#,
        );
        let claims = parse_claims(&format!("header.{payload}.signature")).unwrap();
        assert_eq!(claims.account_id.as_deref(), Some("acct_123"));
        assert_eq!(claims.email.as_deref(), Some("user@example.com"));
        assert_eq!(claims.plan.as_deref(), Some("Pro"));
    }

    #[test]
    fn oauth_login_round_trips_only_through_the_global_toml() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let auth = OAuthStore::with_store(ConfigStore::new(path.clone())).unwrap();
        let oauth = ChatGptOAuthConfig {
            access_token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
            expires_at: now_epoch() + 3600,
            account_id: "account-id".into(),
            email: Some("user@example.com".into()),
            plan: Some("Pro".into()),
        };

        auth.save(&oauth).unwrap();

        assert_eq!(auth.load().unwrap(), Some(oauth));
        assert!(auth.is_logged_in());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[chatgpt_oauth]"));
        assert!(text.contains("access-secret"));
        assert!(text.contains("refresh-secret"));

        auth.logout().unwrap();

        assert_eq!(auth.load().unwrap(), None);
        let text = std::fs::read_to_string(path).unwrap();
        assert!(!text.contains("[chatgpt_oauth]"));
        assert!(!text.contains("access-secret"));
        assert!(!text.contains("refresh-secret"));
    }
}
