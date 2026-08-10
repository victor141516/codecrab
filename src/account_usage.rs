use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::Utc;
use reqwest::{Client, Method, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    auth::{OAuthCredentials, OAuthStore},
    config::Config,
    diagnostics::DebugOutput,
    http_debug,
};

const CHATGPT_BACKEND_BASE: &str = "https://chatgpt.com/backend-api";
const USAGE_PATH: &str = "/wham/usage";
const RESET_CREDITS_PATH: &str = "/wham/rate-limit-reset-credits";
const CONSUME_RESET_CREDIT_PATH: &str = "/wham/rate-limit-reset-credits/consume";
const USAGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct UsageWindow {
    pub limit_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_name: Option<String>,
    pub kind: UsageWindowKind,
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub window_duration_seconds: i64,
    pub resets_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UsageWindowKind {
    Primary,
    Secondary,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ResetCredit {
    pub id: String,
    pub reset_type: String,
    pub status: String,
    pub granted_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct ResetCredits {
    pub available_count: i64,
    pub applicable_available_count: i64,
    pub credits: Vec<ResetCredit>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct UsageSnapshot {
    pub plan_type: String,
    pub windows: Vec<UsageWindow>,
    pub reset_credits: ResetCredits,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct UsageState {
    pub available: bool,
    pub stale: bool,
    pub can_reset: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_updated_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<UsageSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UsageState {
    pub(crate) fn hidden() -> Self {
        Self {
            available: false,
            stale: false,
            can_reset: false,
            last_updated_at: None,
            snapshot: None,
            error: None,
        }
    }

    pub(crate) fn empty() -> Self {
        Self {
            available: true,
            stale: true,
            can_reset: false,
            last_updated_at: None,
            snapshot: None,
            error: Some("Usage unavailable".into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResetOutcome {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ResetResponse {
    pub outcome: ResetOutcome,
    pub windows_reset: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ResetResult {
    pub outcome: ResetOutcome,
    pub windows_reset: i64,
    pub usage: UsageState,
}

#[derive(Deserialize)]
struct UsagePayload {
    #[serde(default = "unknown_plan")]
    plan_type: String,
    #[serde(default)]
    rate_limit: Option<RateLimitDetails>,
    #[serde(default)]
    additional_rate_limits: Vec<AdditionalRateLimit>,
    #[serde(default)]
    rate_limit_reset_credits: Option<ResetCreditsSummary>,
}

fn unknown_plan() -> String {
    "unknown".into()
}

#[derive(Deserialize)]
struct RateLimitDetails {
    #[serde(default)]
    primary_window: Option<RateLimitWindowPayload>,
    #[serde(default)]
    secondary_window: Option<RateLimitWindowPayload>,
}

#[derive(Deserialize)]
struct RateLimitWindowPayload {
    used_percent: f64,
    limit_window_seconds: i64,
    reset_at: i64,
}

#[derive(Deserialize)]
struct AdditionalRateLimit {
    metered_feature: String,
    #[serde(default)]
    limit_name: Option<String>,
    rate_limit: RateLimitDetails,
}

#[derive(Deserialize)]
struct ResetCreditsSummary {
    available_count: i64,
    #[serde(default)]
    applicable_available_count: Option<i64>,
}

#[derive(Deserialize)]
struct ResetCreditDetailsPayload {
    #[serde(default)]
    credits: Vec<ResetCredit>,
    #[serde(default)]
    available_count: i64,
}

#[derive(Deserialize)]
struct ResetResponsePayload {
    code: ResetOutcome,
    #[serde(default)]
    windows_reset: i64,
}

pub(crate) fn usage_available(config: &Config, provider_name: &str, oauth_logged_in: bool) -> bool {
    let Ok(provider) = config.provider(provider_name) else {
        return false;
    };
    if !provider.is_official_openai() || !oauth_logged_in {
        return false;
    }
    matches!(
        provider.auth.trim().to_ascii_lowercase().as_str(),
        "auto" | "oauth"
    )
}

fn parse_usage_payload(value: Value) -> Result<UsageSnapshot> {
    let payload: UsagePayload =
        serde_json::from_value(value).context("invalid ChatGPT usage response")?;
    let summary = payload.rate_limit_reset_credits;
    let mut snapshot = UsageSnapshot {
        plan_type: payload.plan_type,
        windows: Vec::new(),
        reset_credits: ResetCredits {
            available_count: summary.as_ref().map_or(0, |value| value.available_count),
            applicable_available_count: summary
                .as_ref()
                .and_then(|value| value.applicable_available_count)
                .unwrap_or_else(|| summary.as_ref().map_or(0, |value| value.available_count)),
            credits: Vec::new(),
        },
    };
    append_windows(&mut snapshot.windows, "codex", None, payload.rate_limit);
    for additional in payload.additional_rate_limits {
        append_windows(
            &mut snapshot.windows,
            &additional.metered_feature,
            additional.limit_name,
            Some(additional.rate_limit),
        );
    }
    Ok(snapshot)
}

fn append_windows(
    windows: &mut Vec<UsageWindow>,
    limit_id: &str,
    limit_name: Option<String>,
    details: Option<RateLimitDetails>,
) {
    let Some(details) = details else {
        return;
    };
    for (kind, window) in [
        (UsageWindowKind::Primary, details.primary_window),
        (UsageWindowKind::Secondary, details.secondary_window),
    ] {
        let Some(window) = window else {
            continue;
        };
        let used_percent = if window.used_percent.is_finite() {
            window.used_percent.clamp(0.0, 100.0)
        } else {
            0.0
        };
        windows.push(UsageWindow {
            limit_id: limit_id.into(),
            limit_name: limit_name.clone(),
            kind,
            used_percent,
            remaining_percent: 100.0 - used_percent,
            window_duration_seconds: window.limit_window_seconds,
            resets_at: window.reset_at,
        });
    }
}

fn apply_reset_credit_details(snapshot: &mut UsageSnapshot, value: Value) -> Result<()> {
    let details: ResetCreditDetailsPayload =
        serde_json::from_value(value).context("invalid ChatGPT reset-credit response")?;
    snapshot.reset_credits.available_count = details.available_count;
    snapshot.reset_credits.credits = details.credits;
    Ok(())
}

fn parse_reset_response(value: Value) -> Result<ResetResponse> {
    let response: ResetResponsePayload =
        serde_json::from_value(value).context("invalid ChatGPT reset response")?;
    Ok(ResetResponse {
        outcome: response.code,
        windows_reset: response.windows_reset,
    })
}

fn reset_request_payload(idempotency_key: &str, credit_id: Option<&str>) -> Value {
    let mut payload = json!({ "redeem_request_id": idempotency_key });
    if let Some(credit_id) = credit_id {
        payload["credit_id"] = Value::String(credit_id.into());
    }
    payload
}

struct OpenAiUsageClient {
    client: Client,
    auth: OAuthStore,
    base_url: String,
    debug_openai: DebugOutput,
    #[cfg(test)]
    fixed_credentials: Option<OAuthCredentials>,
}

impl OpenAiUsageClient {
    fn new(debug_openai: DebugOutput) -> Result<Self> {
        let mut auth = OAuthStore::new()?;
        auth.set_debug_openai(debug_openai.clone());
        Ok(Self {
            client: Client::builder().timeout(USAGE_REQUEST_TIMEOUT).build()?,
            auth,
            base_url: CHATGPT_BACKEND_BASE.into(),
            debug_openai,
            #[cfg(test)]
            fixed_credentials: None,
        })
    }

    fn is_logged_in(&self) -> bool {
        self.auth.is_logged_in()
    }

    async fn fetch(&self) -> Result<UsageSnapshot> {
        let value = self
            .authenticated_json(Method::GET, USAGE_PATH, None)
            .await?;
        let mut snapshot = parse_usage_payload(value)?;
        if snapshot.reset_credits.available_count > 0 {
            let details = self
                .authenticated_json(Method::GET, RESET_CREDITS_PATH, None)
                .await?;
            apply_reset_credit_details(&mut snapshot, details)?;
        }
        Ok(snapshot)
    }

    async fn consume(
        &self,
        idempotency_key: &str,
        credit_id: Option<&str>,
    ) -> Result<ResetResponse> {
        let payload = reset_request_payload(idempotency_key, credit_id);
        let value = self
            .authenticated_json(Method::POST, CONSUME_RESET_CREDIT_PATH, Some(payload))
            .await?;
        parse_reset_response(value)
    }

    async fn authenticated_json(
        &self,
        method: Method,
        path: &str,
        payload: Option<Value>,
    ) -> Result<Value> {
        #[cfg(test)]
        if let Some(credentials) = &self.fixed_credentials {
            let response = self
                .send(method, path, payload.as_ref(), credentials)
                .await?;
            return self.decode(response).await;
        }
        let credentials = self.auth.credentials().await?;
        let mut response = self
            .send(method.clone(), path, payload.as_ref(), &credentials)
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.log_discarded(response).await?;
            let credentials = self.auth.refresh_credentials().await?;
            response = self
                .send(method, path, payload.as_ref(), &credentials)
                .await?;
        }
        self.decode(response).await
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        payload: Option<&Value>,
        credentials: &OAuthCredentials,
    ) -> Result<Response> {
        let url = format!("{}{path}", self.base_url);
        let mut request = self
            .client
            .request(method, &url)
            .bearer_auth(&credentials.access_token)
            .header("ChatGPT-Account-Id", &credentials.account_id)
            .header("originator", "codecrab")
            .header(
                "User-Agent",
                format!(
                    "codecrab/{} ({}; {})",
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            );
        if let Some(payload) = payload {
            request = request.json(payload);
        }
        let request = request
            .build()
            .context("cannot build ChatGPT usage request")?;
        http_debug::request(&self.debug_openai, &request)?;
        self.client
            .execute(request)
            .await
            .context("ChatGPT usage request failed")
    }

    async fn log_discarded(&self, response: Response) -> Result<()> {
        if self.debug_openai.is_enabled() {
            let _ = self.decode(response).await;
        }
        Ok(())
    }

    async fn decode(&self, response: Response) -> Result<Value> {
        let status = response.status();
        let version = response.version();
        let url = response.url().clone();
        let headers = response.headers().clone();
        let body = response
            .text()
            .await
            .context("cannot read ChatGPT usage response")?;
        http_debug::response(&self.debug_openai, &url, version, status, &headers, &body)?;
        if !status.is_success() {
            anyhow::bail!(
                "ChatGPT usage returned {status}: {}",
                compact_response(&body)
            );
        }
        serde_json::from_str(&body).context("ChatGPT usage returned invalid JSON")
    }
}

fn compact_response(body: &str) -> String {
    let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = compact.chars();
    let shortened = chars.by_ref().take(240).collect::<String>();
    if chars.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}

fn apply_fetch_result(state: &mut UsageState, result: Result<UsageSnapshot>) {
    match result {
        Ok(snapshot) => {
            let can_reset = snapshot.reset_credits.available_count > 0;
            *state = UsageState {
                available: true,
                stale: false,
                can_reset,
                last_updated_at: Some(Utc::now().timestamp()),
                snapshot: Some(snapshot),
                error: None,
            };
        }
        Err(error) => {
            state.available = true;
            state.stale = true;
            state.can_reset = false;
            state.error = Some(format!("Usage unavailable: {error:#}"));
        }
    }
}

fn validate_reset_state(state: &UsageState, replay: bool) -> Result<()> {
    if replay {
        return Ok(());
    }
    let snapshot = state
        .snapshot
        .as_ref()
        .context("usage must be refreshed before resetting it")?;
    if state.stale {
        anyhow::bail!("usage is stale; refresh it before resetting");
    }
    if snapshot.reset_credits.available_count <= 0 {
        anyhow::bail!("no reset credits are available");
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct UsageTracker {
    client: Arc<OpenAiUsageClient>,
    oauth_logged_in: bool,
    state: Arc<Mutex<UsageState>>,
    operation: Arc<Mutex<()>>,
    attempted_reset_keys: Arc<Mutex<HashSet<String>>>,
    requested_refresh: Arc<AtomicU64>,
    completed_refresh: Arc<AtomicU64>,
}

impl UsageTracker {
    pub(crate) fn new(debug_openai: DebugOutput) -> Result<Self> {
        Self::build(debug_openai, None, None)
    }

    fn build(
        debug_openai: DebugOutput,
        oauth_logged_in: Option<bool>,
        base_url: Option<String>,
    ) -> Result<Self> {
        let mut client = OpenAiUsageClient::new(debug_openai)?;
        if let Some(base_url) = base_url {
            client.base_url = base_url;
        }
        #[cfg(test)]
        if oauth_logged_in == Some(true) {
            client.fixed_credentials = Some(OAuthCredentials {
                access_token: "test-token".into(),
                account_id: "test-account".into(),
            });
        }
        let oauth_logged_in = oauth_logged_in.unwrap_or_else(|| client.is_logged_in());
        Ok(Self {
            client: Arc::new(client),
            oauth_logged_in,
            state: Arc::new(Mutex::new(UsageState::empty())),
            operation: Arc::new(Mutex::new(())),
            attempted_reset_keys: Arc::new(Mutex::new(HashSet::new())),
            requested_refresh: Arc::new(AtomicU64::new(0)),
            completed_refresh: Arc::new(AtomicU64::new(0)),
        })
    }

    #[cfg(test)]
    pub(crate) fn test(oauth_logged_in: bool, base_url: Option<String>) -> Result<Self> {
        Self::build(DebugOutput::default(), Some(oauth_logged_in), base_url)
    }

    pub(crate) fn refresh_in_background(&self, config: Config, provider_name: String) {
        if !self.available_for(&config, &provider_name) {
            return;
        }
        let request = self.request_refresh();
        let tracker = self.clone();
        tokio::spawn(async move {
            tracker
                .run_requested_refresh(&config, &provider_name, request)
                .await;
        });
    }

    pub(crate) async fn latest(&self) -> UsageState {
        self.state.lock().await.clone()
    }

    pub(crate) fn available_for(&self, config: &Config, provider_name: &str) -> bool {
        usage_available(config, provider_name, self.oauth_logged_in)
    }

    pub(crate) async fn current_for(&self, config: &Config, provider_name: &str) -> UsageState {
        if !self.available_for(config, provider_name) {
            return UsageState::hidden();
        }
        self.latest().await
    }

    pub(crate) async fn refresh_for(&self, config: &Config, provider_name: &str) -> UsageState {
        if !self.available_for(config, provider_name) {
            return UsageState::hidden();
        }
        let request = self.request_refresh();
        self.run_requested_refresh(config, provider_name, request)
            .await
    }

    pub(crate) async fn refresh_coalesced_for(
        &self,
        config: &Config,
        provider_name: &str,
    ) -> UsageState {
        if !self.available_for(config, provider_name) {
            return UsageState::hidden();
        }
        let request = self.requested_refresh.load(Ordering::Acquire);
        if self.completed_refresh.load(Ordering::Acquire) >= request {
            return self.latest().await;
        }
        self.run_requested_refresh(config, provider_name, request)
            .await
    }

    fn request_refresh(&self) -> u64 {
        self.requested_refresh.fetch_add(1, Ordering::AcqRel) + 1
    }

    async fn run_requested_refresh(
        &self,
        config: &Config,
        provider_name: &str,
        request: u64,
    ) -> UsageState {
        if !self.available_for(config, provider_name) {
            return UsageState::hidden();
        }
        let _operation = self.operation.lock().await;
        if self.completed_refresh.load(Ordering::Acquire) >= request {
            return self.latest().await;
        }
        let result = self.client.fetch().await;
        let mut state = self.state.lock().await;
        apply_fetch_result(&mut state, result);
        self.completed_refresh.store(request, Ordering::Release);
        state.clone()
    }

    pub(crate) async fn reset_for(
        &self,
        config: &Config,
        provider_name: &str,
        idempotency_key: &str,
        credit_id: Option<&str>,
    ) -> Result<ResetResult> {
        if !self.available_for(config, provider_name) {
            anyhow::bail!("OpenAI usage is not available for the selected provider");
        }
        if idempotency_key.trim().is_empty() {
            anyhow::bail!("an idempotency key is required");
        }
        let _operation = self.operation.lock().await;
        let replay = self
            .attempted_reset_keys
            .lock()
            .await
            .contains(idempotency_key);
        {
            let state = self.state.lock().await;
            validate_reset_state(&state, replay)?;
        }
        if !replay {
            self.attempted_reset_keys
                .lock()
                .await
                .insert(idempotency_key.to_owned());
        }

        let response = match self.client.consume(idempotency_key, credit_id).await {
            Ok(response) => response,
            Err(error) => {
                let mut state = self.state.lock().await;
                state.stale = true;
                state.can_reset = false;
                state.error = Some(format!("Usage unavailable: {error:#}"));
                return Err(error);
            }
        };
        let result = self.client.fetch().await;
        let mut state = self.state.lock().await;
        apply_fetch_result(&mut state, result);
        let requested = self.requested_refresh.load(Ordering::Acquire);
        self.completed_refresh
            .fetch_max(requested, Ordering::AcqRel);
        Ok(ResetResult {
            outcome: response.outcome,
            windows_reset: response.windows_reset,
            usage: state.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DEFAULT_PROVIDER, OFFICIAL_OPENAI_BASE_URL};
    use serde_json::json;
    use tokio::{io::AsyncWriteExt, net::TcpListener};

    #[test]
    fn parses_provider_defined_windows_and_reset_credits() {
        let snapshot = parse_usage_payload(json!({
            "plan_type": "prolite",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 37.5,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 3600,
                    "reset_at": 1786826526
                },
                "secondary_window": null
            },
            "additional_rate_limits": [{
                "limit_name": "GPT-5.3-Codex-Spark",
                "metered_feature": "codex_bengalfox",
                "rate_limit": {
                    "allowed": true,
                    "limit_reached": false,
                    "primary_window": {
                        "used_percent": 5,
                        "limit_window_seconds": 604800,
                        "reset_after_seconds": 7200,
                        "reset_at": 1786830126
                    },
                    "secondary_window": null
                }
            }],
            "rate_limit_reset_credits": {
                "available_count": 2,
                "applicable_available_count": 1
            }
        }))
        .unwrap();

        assert_eq!(snapshot.plan_type, "prolite");
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].limit_id, "codex");
        assert_eq!(snapshot.windows[0].used_percent, 37.5);
        assert_eq!(snapshot.windows[0].remaining_percent, 62.5);
        assert_eq!(snapshot.windows[0].window_duration_seconds, 604800);
        assert_eq!(snapshot.windows[0].resets_at, 1786826526);
        assert_eq!(snapshot.windows[1].limit_id, "codex_bengalfox");
        assert_eq!(
            snapshot.windows[1].limit_name.as_deref(),
            Some("GPT-5.3-Codex-Spark")
        );
        assert_eq!(snapshot.reset_credits.available_count, 2);
        assert_eq!(snapshot.reset_credits.applicable_available_count, 1);
    }

    #[test]
    fn parses_reset_credit_details_and_ignores_profile_metadata() {
        let mut snapshot = UsageSnapshot {
            plan_type: "plus".into(),
            windows: Vec::new(),
            reset_credits: ResetCredits::default(),
        };
        apply_reset_credit_details(
            &mut snapshot,
            json!({
                "credits": [{
                    "id": "credit-1",
                    "reset_type": "codex_rate_limits",
                    "status": "available",
                    "granted_at": "2026-06-17T00:00:00Z",
                    "expires_at": "2026-07-17T00:00:00Z",
                    "profile_user_id": "ignored",
                    "title": "Full reset",
                    "description": "Ready to redeem"
                }],
                "available_count": 1,
                "total_earned_count": 4
            }),
        )
        .unwrap();

        assert_eq!(snapshot.reset_credits.available_count, 1);
        assert_eq!(snapshot.reset_credits.credits.len(), 1);
        assert_eq!(snapshot.reset_credits.credits[0].id, "credit-1");
        assert_eq!(
            snapshot.reset_credits.credits[0].title.as_deref(),
            Some("Full reset")
        );
    }

    #[test]
    fn availability_requires_official_openai_oauth() {
        let mut config = Config::default();
        {
            let provider = config.providers.get_mut(DEFAULT_PROVIDER).unwrap();
            provider.base_url = OFFICIAL_OPENAI_BASE_URL.into();
            provider.auth = "auto".into();
        }
        assert!(usage_available(&config, DEFAULT_PROVIDER, true));
        assert!(!usage_available(&config, DEFAULT_PROVIDER, false));

        config.providers.get_mut(DEFAULT_PROVIDER).unwrap().auth = "oauth".into();
        assert!(usage_available(&config, DEFAULT_PROVIDER, true));

        config.providers.get_mut(DEFAULT_PROVIDER).unwrap().auth = "api_key".into();
        assert!(!usage_available(&config, DEFAULT_PROVIDER, true));

        {
            let provider = config.providers.get_mut(DEFAULT_PROVIDER).unwrap();
            provider.auth = "auto".into();
            provider.base_url = "https://compatible.example/v1".into();
        }
        assert!(!usage_available(&config, DEFAULT_PROVIDER, true));
    }

    #[test]
    fn parses_every_reset_outcome_without_inventing_success() {
        for (code, expected) in [
            ("reset", ResetOutcome::Reset),
            ("nothing_to_reset", ResetOutcome::NothingToReset),
            ("no_credit", ResetOutcome::NoCredit),
            ("already_redeemed", ResetOutcome::AlreadyRedeemed),
        ] {
            let response = parse_reset_response(json!({
                "code": code,
                "windows_reset": 2
            }))
            .unwrap();
            assert_eq!(response.outcome, expected);
            assert_eq!(response.windows_reset, 2);
        }
    }

    #[test]
    fn reset_payload_uses_the_provider_idempotency_contract() {
        assert_eq!(
            reset_request_payload("request-123", Some("credit-456")),
            json!({
                "redeem_request_id": "request-123",
                "credit_id": "credit-456"
            })
        );
        assert_eq!(
            reset_request_payload("request-123", None),
            json!({ "redeem_request_id": "request-123" })
        );
    }

    #[test]
    fn failed_refresh_preserves_last_known_usage_as_stale() {
        let mut state = UsageState {
            available: true,
            stale: false,
            can_reset: false,
            last_updated_at: Some(123),
            snapshot: Some(UsageSnapshot {
                plan_type: "plus".into(),
                windows: Vec::new(),
                reset_credits: ResetCredits::default(),
            }),
            error: None,
        };

        apply_fetch_result(&mut state, Err(anyhow::anyhow!("contract changed")));

        assert!(state.available);
        assert!(state.stale);
        assert_eq!(state.last_updated_at, Some(123));
        assert_eq!(state.snapshot.unwrap().plan_type, "plus");
        assert!(state.error.unwrap().contains("contract changed"));
    }

    #[test]
    fn only_an_attempted_idempotency_key_can_bypass_stale_preconditions() {
        let stale = UsageState::empty();
        assert!(validate_reset_state(&stale, false).is_err());
        assert!(validate_reset_state(&stale, true).is_ok());
        assert!(!stale.can_reset);
    }

    #[tokio::test]
    async fn private_request_contract_uses_exact_route_and_account_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = crate::test_support::read_http_request(&mut socket).await;
            let response = r#"{"code":"reset","windows_reset":1}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });

        let mut client = OpenAiUsageClient::new(DebugOutput::default()).unwrap();
        client.base_url = format!("http://{address}");
        let payload = reset_request_payload("request-123", Some("credit-456"));
        let response = client
            .send(
                Method::POST,
                CONSUME_RESET_CREDIT_PATH,
                Some(&payload),
                &OAuthCredentials {
                    access_token: "secret-token".into(),
                    account_id: "account-123".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            parse_reset_response(client.decode(response).await.unwrap())
                .unwrap()
                .outcome,
            ResetOutcome::Reset
        );

        let request = server.await.unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        let headers = headers.to_ascii_lowercase();
        assert!(headers.starts_with("post /wham/rate-limit-reset-credits/consume http/1.1"));
        assert!(headers.contains("authorization: bearer secret-token"));
        assert!(headers.contains("chatgpt-account-id: account-123"));
        assert!(headers.contains("originator: codecrab"));
        assert_eq!(serde_json::from_str::<Value>(body).unwrap(), payload);
    }

    #[tokio::test]
    async fn ambiguous_reset_replays_the_same_key_despite_stale_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let replies = [
                (500, r#"{"error":"response lost"}"#),
                (200, r#"{"code":"already_redeemed","windows_reset":1}"#),
                (
                    200,
                    r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":0,"limit_window_seconds":604800,"reset_at":1787431326}},"rate_limit_reset_credits":{"available_count":0}}"#,
                ),
            ];
            let mut requests = Vec::new();
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.push(crate::test_support::read_http_request(&mut socket).await);
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            requests
        });

        let mut config = Config::default();
        let provider = config.providers.get_mut(DEFAULT_PROVIDER).unwrap();
        provider.base_url = OFFICIAL_OPENAI_BASE_URL.into();
        provider.auth = "oauth".into();
        let tracker = UsageTracker::test(true, Some(format!("http://{address}"))).unwrap();
        *tracker.state.lock().await = UsageState {
            available: true,
            stale: false,
            can_reset: true,
            last_updated_at: Some(123),
            snapshot: Some(UsageSnapshot {
                plan_type: "plus".into(),
                windows: Vec::new(),
                reset_credits: ResetCredits {
                    available_count: 1,
                    applicable_available_count: 1,
                    credits: Vec::new(),
                },
            }),
            error: None,
        };

        assert!(
            tracker
                .reset_for(&config, DEFAULT_PROVIDER, "request-123", None)
                .await
                .is_err()
        );
        let stale = tracker.latest().await;
        assert!(stale.stale);
        assert!(!stale.can_reset);

        let replay = tracker
            .reset_for(&config, DEFAULT_PROVIDER, "request-123", None)
            .await
            .unwrap();
        assert_eq!(replay.outcome, ResetOutcome::AlreadyRedeemed);
        assert!(!replay.usage.stale);

        let requests = server.await.unwrap();
        for request in &requests[..2] {
            let request = String::from_utf8(request.clone()).unwrap();
            assert!(request.contains(r#""redeem_request_id":"request-123""#));
        }
    }

    #[tokio::test]
    async fn post_turn_and_client_refreshes_share_one_private_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = crate::test_support::read_http_request(&mut socket).await;
            let body = r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":20,"limit_window_seconds":604800,"reset_at":1787431326}},"rate_limit_reset_credits":{"available_count":0}}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            drop(socket);
            let duplicate = tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_ok();
            (request, duplicate)
        });
        let mut config = Config::default();
        let provider = config.providers.get_mut(DEFAULT_PROVIDER).unwrap();
        provider.base_url = OFFICIAL_OPENAI_BASE_URL.into();
        provider.auth = "oauth".into();
        let tracker = UsageTracker::test(true, Some(format!("http://{address}"))).unwrap();

        tracker.refresh_in_background(config.clone(), DEFAULT_PROVIDER.into());
        let state = tokio::time::timeout(
            Duration::from_secs(2),
            tracker.refresh_coalesced_for(&config, DEFAULT_PROVIDER),
        )
        .await
        .unwrap();

        assert!(!state.stale);
        assert_eq!(state.snapshot.unwrap().windows[0].remaining_percent, 80.0);
        let (request, duplicate) = server.await.unwrap();
        assert!(!duplicate);
        assert!(
            String::from_utf8(request)
                .unwrap()
                .starts_with("GET /wham/usage")
        );
    }
}
