use std::{collections::BTreeMap, time::Duration};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth::{OAuthCredentials, OAuthStore},
    config::{CatalogOptionConfig, Config, ModelCapabilitiesConfig},
    http_debug,
};

const CHATGPT_CODEX_BASE: &str = "https://chatgpt.com/backend-api/codex";
const CHATGPT_CODEX_RESPONSES: &str = "https://chatgpt.com/backend-api/codex/responses";
// This is a Codex protocol compatibility version, not CodeCrab's package
// version. The neutral value requests the account's compatible catalog.
const CODEX_CATALOG_COMPAT_VERSION: &str = "0.0.0";
const PREFERRED_DEFAULT_MODEL: &str = "gpt-5.6-sol";
const PREFERRED_DEFAULT_REASONING: &str = "high";
const PREFERRED_DEFAULT_SPEED: &str = "fast";

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct ReasoningOption {
    pub effort: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct ServiceTierOption {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct ModelCatalogEntry {
    pub slug: String,
    pub display_name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
    pub supported_reasoning_levels: Vec<ReasoningOption>,
    pub visibility: String,
    pub supported_in_api: bool,
    pub priority: i32,
    #[serde(default)]
    pub service_tiers: Vec<ServiceTierOption>,
    #[serde(default)]
    pub default_service_tier: Option<String>,
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub output_modalities: Vec<String>,
}

impl ModelCatalogEntry {
    pub(crate) fn from_id(id: String) -> Self {
        Self {
            display_name: id.clone(),
            slug: id,
            description: None,
            default_reasoning_level: None,
            supported_reasoning_levels: Vec::new(),
            visibility: "list".into(),
            supported_in_api: true,
            priority: 0,
            service_tiers: Vec::new(),
            default_service_tier: None,
            input_modalities: Vec::new(),
            output_modalities: Vec::new(),
        }
    }

    pub(crate) fn label(&self) -> &str {
        &self.display_name
    }

    pub(crate) fn available_service_tiers(&self) -> Vec<ServiceTierOption> {
        self.service_tiers
            .iter()
            .filter(|tier| tier.id != "default")
            .cloned()
            .collect()
    }
}

#[derive(Clone)]
pub(crate) struct ModelSelection {
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
}

pub(crate) fn default_model_selection(catalog: &[ModelCatalogEntry]) -> Option<ModelSelection> {
    let model = catalog
        .iter()
        .find(|model| model.slug == PREFERRED_DEFAULT_MODEL)
        .or_else(|| catalog.first())?;
    let use_preferred_settings = model.slug == PREFERRED_DEFAULT_MODEL;
    let reasoning_effort = use_preferred_settings
        .then(|| {
            model
                .supported_reasoning_levels
                .iter()
                .find(|option| {
                    option
                        .effort
                        .eq_ignore_ascii_case(PREFERRED_DEFAULT_REASONING)
                })
                .map(|option| option.effort.clone())
        })
        .flatten()
        .or_else(|| model.default_reasoning_level.clone());
    let service_tier = use_preferred_settings
        .then(|| {
            model
                .available_service_tiers()
                .into_iter()
                .find(|tier| tier.name.eq_ignore_ascii_case(PREFERRED_DEFAULT_SPEED))
                .map(|tier| tier.id)
        })
        .flatten()
        .or_else(|| {
            model
                .default_service_tier
                .clone()
                .filter(|tier| tier != "default")
        });
    Some(ModelSelection {
        model: model.slug.clone(),
        reasoning_effort,
        service_tier,
    })
}

#[derive(Deserialize)]
struct CodexModelsResponse {
    models: Vec<ModelCatalogEntry>,
}

#[derive(Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModel>,
}

#[derive(Deserialize)]
struct OpenAiModel {
    id: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
}

impl Message {
    pub(crate) fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            sequence: None,
            created_at: Some(Utc::now()),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            hidden: false,
        }
    }

    pub(crate) fn hidden_text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            sequence: None,
            created_at: Some(Utc::now()),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            hidden: true,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    tools: &'a [Value],
    tool_choice: &'static str,
    parallel_tool_calls: bool,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<&'a str>,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<&'a Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a String>,
}

impl<'a> From<&'a Message> for ChatMessage<'a> {
    fn from(message: &'a Message) -> Self {
        Self {
            role: &message.role,
            content: message.content.as_ref(),
            tool_calls: message.tool_calls.as_ref(),
            tool_call_id: message.tool_call_id.as_ref(),
        }
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

enum Backend {
    ChatCompletions {
        base_url: String,
        api_key: Option<String>,
    },
    ChatGptSubscription {
        auth: OAuthStore,
    },
}

pub(crate) struct OpenAiCompatible {
    client: Client,
    backend: Backend,
    stream_idle_timeout: Duration,
    model: String,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    session_id: Uuid,
    debug_openai: bool,
    fetch_models: bool,
    allowed_models: Option<Vec<String>>,
    model_capabilities: BTreeMap<String, ModelCapabilitiesConfig>,
}

impl OpenAiCompatible {
    pub(crate) fn new(config: &Config, provider_name: &str) -> Result<Self> {
        let provider = config.provider(provider_name)?;
        let client = Client::builder().build()?;
        let official_openai = provider.is_official_openai();
        let auth_mode = provider.auth.trim().to_ascii_lowercase();
        let oauth = OAuthStore::new()?;
        let use_oauth = match auth_mode.as_str() {
            "auto" => official_openai && oauth.is_logged_in(),
            "oauth" => {
                if !official_openai {
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
            Backend::ChatGptSubscription { auth: oauth }
        } else {
            let api_key = match auth_mode.as_str() {
                "none" => None,
                "auto" | "api_key" if !provider.api_key.is_empty() => {
                    Some(provider.api_key.clone())
                }
                "auto" if official_openai => anyhow::bail!(
                    "not authenticated; run `codecrab auth login` to use ChatGPT Pro, or configure providers.{provider_name}.api_key"
                ),
                "auto" | "api_key" => {
                    anyhow::bail!("provider {provider_name:?} does not have an API key configured")
                }
                _ => unreachable!("OAuth was handled above"),
            };
            Backend::ChatCompletions {
                base_url: provider.base_url.trim_end_matches('/').to_owned(),
                api_key,
            }
        };

        Ok(Self {
            client,
            backend,
            stream_idle_timeout: Duration::from_secs(config.request_timeout_seconds),
            model: provider.model.clone(),
            reasoning_effort: None,
            service_tier: None,
            session_id: Uuid::new_v4(),
            debug_openai: false,
            fetch_models: provider.fetch_models,
            allowed_models: provider.allowed_models.clone(),
            model_capabilities: provider.model_capabilities.clone(),
        })
    }

    pub(crate) fn set_debug_openai(&mut self, enabled: bool) {
        self.debug_openai = enabled;
        if let Backend::ChatGptSubscription { auth } = &mut self.backend {
            auth.set_debug_openai(enabled);
        }
    }

    pub(crate) fn set_selection(&mut self, selection: &ModelSelection) {
        self.model.clone_from(&selection.model);
        self.reasoning_effort
            .clone_from(&selection.reasoning_effort);
        self.service_tier.clone_from(&selection.service_tier);
    }

    fn selected_model_is_allowed(&self) -> Result<()> {
        if let Some(allowed) = &self.allowed_models
            && self.model != "auto"
            && !allowed.contains(&self.model)
        {
            anyhow::bail!(
                "model {:?} is not present in this provider's allowed_models",
                self.model
            );
        }
        Ok(())
    }

    pub(crate) async fn fetch_models(&self) -> Result<Vec<ModelCatalogEntry>> {
        let models = if !self.fetch_models {
            Vec::new()
        } else {
            match &self.backend {
                Backend::ChatCompletions {
                    base_url, api_key, ..
                } => {
                    let url = format!("{base_url}/models");
                    let response = self
                        .send_models_request(&url, api_key.as_deref(), None)
                        .await?;
                    parse_models_response(response, false, self.debug_openai).await?
                }
                Backend::ChatGptSubscription { auth } => {
                    let url = format!("{CHATGPT_CODEX_BASE}/models");
                    let credentials = auth.credentials().await?;
                    let mut response = self
                        .send_models_request(
                            &url,
                            Some(&credentials.access_token),
                            Some(&credentials),
                        )
                        .await?;
                    if response.status() == StatusCode::UNAUTHORIZED {
                        if self.debug_openai {
                            log_discarded_response(response, Duration::from_secs(8)).await?;
                        }
                        let credentials = auth.refresh_credentials().await?;
                        response = self
                            .send_models_request(
                                &url,
                                Some(&credentials.access_token),
                                Some(&credentials),
                            )
                            .await?;
                    }
                    parse_models_response(response, true, self.debug_openai).await?
                }
            }
        };
        merge_model_catalog(
            models,
            &self.model_capabilities,
            self.allowed_models.as_deref(),
        )
    }

    async fn send_models_request(
        &self,
        url: &str,
        bearer: Option<&str>,
        credentials: Option<&OAuthCredentials>,
    ) -> Result<Response> {
        let mut request = self
            .client
            .get(url)
            .query(&[("client_version", CODEX_CATALOG_COMPAT_VERSION)])
            .timeout(Duration::from_secs(8))
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
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(credentials) = credentials {
            request = request.header("ChatGPT-Account-Id", &credentials.account_id);
        }
        let request = request
            .build()
            .context("cannot build model catalog request")?;
        http_debug::request(self.debug_openai, &request);
        self.client
            .execute(request)
            .await
            .context("model catalog request failed")
    }

    pub(crate) async fn complete(
        &self,
        messages: &[Message],
        tools: &[Value],
        mut on_text_delta: impl FnMut(&str),
    ) -> Result<Message> {
        self.selected_model_is_allowed()?;
        match &self.backend {
            Backend::ChatCompletions { base_url, api_key } => {
                self.complete_chat(
                    base_url,
                    api_key.as_deref(),
                    messages,
                    tools,
                    &mut on_text_delta,
                )
                .await
            }
            Backend::ChatGptSubscription { auth } => {
                self.complete_subscription(auth, messages, tools, &mut on_text_delta)
                    .await
            }
        }
    }

    async fn complete_chat(
        &self,
        base_url: &str,
        api_key: Option<&str>,
        messages: &[Message],
        tools: &[Value],
        on_text_delta: &mut impl FnMut(&str),
    ) -> Result<Message> {
        let mut request = self
            .client
            .post(format!("{base_url}/chat/completions"))
            .json(&ChatRequest {
                model: &self.model,
                messages: messages.iter().map(ChatMessage::from).collect(),
                tools,
                tool_choice: "auto",
                parallel_tool_calls: true,
                stream: true,
                reasoning_effort: self.reasoning_effort.as_deref(),
                service_tier: self.service_tier.as_deref(),
            });
        if let Some(api_key) = api_key {
            request = request.bearer_auth(api_key);
        }
        let request = request.build().context("cannot build model request")?;
        http_debug::request(self.debug_openai, &request);
        let response = self.execute_model_request(request, "model request").await?;
        let status = response.status();
        let version = response.version();
        let url = response.url().clone();
        let headers = response.headers().clone();
        if !status.is_success() {
            let body = read_response_body(response, self.stream_idle_timeout).await?;
            http_debug::response(self.debug_openai, &url, version, status, &headers, &body);
            anyhow::bail!("model returned {status}: {}", compact_error(&body));
        }
        if !is_event_stream(&headers) {
            let body = read_response_body(response, self.stream_idle_timeout).await?;
            http_debug::response(self.debug_openai, &url, version, status, &headers, &body);
            let parsed: ChatResponse =
                serde_json::from_str(&body).context("model returned an invalid response")?;
            let message = parsed
                .choices
                .into_iter()
                .next()
                .map(|choice| choice.message)
                .context("model returned no choices")?;
            return Ok(message);
        }
        let mut stream = ChatCompletionStream::default();
        let body = read_sse(response, self.stream_idle_timeout, |data| {
            stream.push(data, on_text_delta)
        })
        .await?;
        http_debug::response(self.debug_openai, &url, version, status, &headers, &body);
        stream.finish()
    }

    async fn complete_subscription(
        &self,
        auth: &OAuthStore,
        messages: &[Message],
        tools: &[Value],
        on_text_delta: &mut impl FnMut(&str),
    ) -> Result<Message> {
        let payload = responses_payload(
            &self.model,
            self.reasoning_effort.as_deref(),
            self.service_tier.as_deref(),
            messages,
            tools,
            self.session_id,
        );
        let credentials = auth.credentials().await?;
        let mut response = self.send_subscription(&credentials, &payload).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            if self.debug_openai {
                log_discarded_response(response, self.stream_idle_timeout).await?;
            }
            let credentials = auth.refresh_credentials().await?;
            response = self.send_subscription(&credentials, &payload).await?;
        }
        let status = response.status();
        let version = response.version();
        let url = response.url().clone();
        let headers = response.headers().clone();
        if !status.is_success() {
            let body = read_response_body(response, self.stream_idle_timeout).await?;
            http_debug::response(self.debug_openai, &url, version, status, &headers, &body);
            anyhow::bail!(
                "ChatGPT subscription returned {status}: {}",
                compact_error(&body)
            );
        }
        let mut stream = ResponsesStream::default();
        let body = read_sse(response, self.stream_idle_timeout, |data| {
            stream.push(data, on_text_delta)
        })
        .await?;
        http_debug::response(self.debug_openai, &url, version, status, &headers, &body);
        stream.finish()
    }

    async fn send_subscription(
        &self,
        credentials: &OAuthCredentials,
        payload: &Value,
    ) -> Result<Response> {
        let request = self
            .client
            .post(CHATGPT_CODEX_RESPONSES)
            .bearer_auth(&credentials.access_token)
            .header("ChatGPT-Account-Id", &credentials.account_id)
            .header("Accept", "text/event-stream")
            .header("originator", "codecrab")
            .header("session-id", self.session_id.to_string())
            .header(
                "User-Agent",
                format!(
                    "codecrab/{} ({}; {})",
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            )
            .json(payload)
            .build()
            .context("cannot build ChatGPT subscription request")?;
        http_debug::request(self.debug_openai, &request);
        self.execute_model_request(request, "ChatGPT subscription request")
            .await
    }

    async fn execute_model_request(
        &self,
        request: reqwest::Request,
        description: &str,
    ) -> Result<Response> {
        match tokio::time::timeout(self.stream_idle_timeout, self.client.execute(request)).await {
            Ok(response) => response.with_context(|| format!("{description} failed")),
            Err(_) => anyhow::bail!(
                "{description} received no data for {} seconds",
                self.stream_idle_timeout.as_secs()
            ),
        }
    }
}

async fn log_discarded_response(response: Response, idle_timeout: Duration) -> Result<()> {
    let status = response.status();
    let version = response.version();
    let url = response.url().clone();
    let headers = response.headers().clone();
    let body = read_response_body(response, idle_timeout).await?;
    http_debug::response(true, &url, version, status, &headers, &body);
    Ok(())
}

fn responses_payload(
    model: &str,
    reasoning_effort: Option<&str>,
    service_tier: Option<&str>,
    messages: &[Message],
    tools: &[Value],
    session_id: Uuid,
) -> Value {
    let instructions = messages
        .iter()
        .filter(|message| matches!(message.role, Role::System))
        .filter_map(|message| message.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut input = Vec::new();
    for message in messages {
        match message.role {
            Role::System => {}
            Role::User => {
                if let Some(content) = &message.content {
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": content}]
                    }));
                }
            }
            Role::Assistant => {
                if let Some(content) = message.content.as_deref().filter(|text| !text.is_empty()) {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": content}]
                    }));
                }
                if let Some(calls) = &message.tool_calls {
                    for call in calls {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.function.name,
                            "arguments": call.function.arguments
                        }));
                    }
                }
            }
            Role::Tool => {
                if let (Some(call_id), Some(content)) = (&message.tool_call_id, &message.content) {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": content
                    }));
                }
            }
        }
    }
    let response_tools = tools
        .iter()
        .filter_map(|tool| tool.get("function"))
        .map(|function| {
            json!({
                "type": "function",
                "name": function.get("name"),
                "description": function.get("description"),
                "parameters": function.get("parameters"),
                "strict": false
            })
        })
        .collect::<Vec<_>>();

    let mut payload = json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "tools": response_tools,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "reasoning": {"summary": "auto"},
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": session_id.to_string(),
        "client_metadata": {
            "session_id": session_id.to_string()
        }
    });
    if let Some(effort) = reasoning_effort {
        payload["reasoning"]["effort"] = json!(effort);
    }
    if let Some(tier) = service_tier {
        payload["service_tier"] = json!(tier);
    }
    payload
}

fn merge_model_catalog(
    mut models: Vec<ModelCatalogEntry>,
    configured: &BTreeMap<String, ModelCapabilitiesConfig>,
    allowed_models: Option<&[String]>,
) -> Result<Vec<ModelCatalogEntry>> {
    for model in &mut models {
        normalize_catalog_options(model);
        if let Some(capabilities) = configured.get(&model.slug) {
            merge_model_capabilities(model, capabilities);
        }
    }
    for (id, capabilities) in configured {
        if models.iter().any(|model| model.slug == *id) {
            continue;
        }
        let mut model = ModelCatalogEntry::from_id(id.clone());
        merge_model_capabilities(&mut model, capabilities);
        models.push(model);
    }

    if let Some(allowed) = allowed_models {
        let missing = allowed
            .iter()
            .filter(|id| !models.iter().any(|model| model.slug == id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            anyhow::bail!(
                "allowed_models references models that were neither returned by the provider nor declared in model_capabilities: {}",
                missing.join(", ")
            );
        }
        models.retain(|model| allowed.contains(&model.slug));
        models.sort_by_key(|model| allowed.iter().position(|id| id == &model.slug));
    }

    for model in &models {
        if let Some(capabilities) = configured.get(&model.slug) {
            validate_configured_defaults(model, capabilities)?;
        }
    }
    if models.is_empty() {
        anyhow::bail!("the provider catalog contains no configured models");
    }
    Ok(models)
}

fn merge_model_capabilities(model: &mut ModelCatalogEntry, configured: &ModelCapabilitiesConfig) {
    if let Some(display_name) = &configured.display_name {
        model.display_name.clone_from(display_name);
    }
    if let Some(description) = &configured.description {
        model.description = Some(description.clone());
    }
    if let Some(default) = &configured.default_reasoning_level {
        model.default_reasoning_level = Some(default.clone());
    }
    merge_reasoning_options(
        &mut model.supported_reasoning_levels,
        &configured.reasoning_levels,
    );
    if let Some(default) = &configured.default_service_tier {
        model.default_service_tier = Some(default.clone());
    }
    merge_service_tiers(&mut model.service_tiers, &configured.service_tiers);
    merge_strings(&mut model.input_modalities, &configured.input_modalities);
    merge_strings(&mut model.output_modalities, &configured.output_modalities);
    normalize_catalog_options(model);
}

fn merge_reasoning_options(
    existing: &mut Vec<ReasoningOption>,
    configured: &[CatalogOptionConfig],
) {
    for option in configured {
        if let Some(current) = existing
            .iter_mut()
            .find(|current| current.effort == option.id)
        {
            if let Some(name) = &option.name {
                current.name.clone_from(name);
            }
            if let Some(description) = &option.description {
                current.description.clone_from(description);
            }
        } else {
            existing.push(ReasoningOption {
                effort: option.id.clone(),
                name: option.name.clone().unwrap_or_else(|| option.id.clone()),
                description: option.description.clone().unwrap_or_default(),
            });
        }
    }
}

fn merge_service_tiers(existing: &mut Vec<ServiceTierOption>, configured: &[CatalogOptionConfig]) {
    for option in configured {
        if let Some(current) = existing.iter_mut().find(|current| current.id == option.id) {
            if let Some(name) = &option.name {
                current.name.clone_from(name);
            }
            if let Some(description) = &option.description {
                current.description.clone_from(description);
            }
        } else {
            existing.push(ServiceTierOption {
                id: option.id.clone(),
                name: option.name.clone().unwrap_or_else(|| option.id.clone()),
                description: option.description.clone().unwrap_or_default(),
            });
        }
    }
}

fn merge_strings(existing: &mut Vec<String>, configured: &[String]) {
    for value in configured {
        if !existing.contains(value) {
            existing.push(value.clone());
        }
    }
}

fn normalize_catalog_options(model: &mut ModelCatalogEntry) {
    for option in &mut model.supported_reasoning_levels {
        if option.name.is_empty() {
            option.name.clone_from(&option.effort);
        }
    }
    for option in &mut model.service_tiers {
        if option.name.is_empty() {
            option.name.clone_from(&option.id);
        }
    }
}

fn validate_configured_defaults(
    model: &ModelCatalogEntry,
    configured: &ModelCapabilitiesConfig,
) -> Result<()> {
    if let Some(default) = &configured.default_reasoning_level
        && !model
            .supported_reasoning_levels
            .iter()
            .any(|option| option.effort == *default)
    {
        anyhow::bail!(
            "model {:?} configures default_reasoning_level {:?}, but that reasoning level does not exist in the merged catalog",
            model.slug,
            default
        );
    }
    if let Some(default) = &configured.default_service_tier
        && default != "default"
        && !model
            .service_tiers
            .iter()
            .any(|option| option.id == *default)
    {
        anyhow::bail!(
            "model {:?} configures default_service_tier {:?}, but that service tier does not exist in the merged catalog",
            model.slug,
            default
        );
    }
    Ok(())
}

async fn parse_models_response(
    response: Response,
    chatgpt_mode: bool,
    debug_openai: bool,
) -> Result<Vec<ModelCatalogEntry>> {
    let status = response.status();
    let version = response.version();
    let url = response.url().clone();
    let headers = response.headers().clone();
    let body = response.text().await?;
    http_debug::response(debug_openai, &url, version, status, &headers, &body);
    if !status.is_success() {
        anyhow::bail!("model catalog returned {status}: {}", compact_error(&body));
    }
    if let Ok(response) = serde_json::from_str::<CodexModelsResponse>(&body) {
        let catalog_count = response.models.len();
        let api_supported = response
            .models
            .iter()
            .filter(|model| model.supported_in_api)
            .count();
        let mut visibilities = response
            .models
            .iter()
            .map(|model| model.visibility.clone())
            .collect::<Vec<_>>();
        visibilities.sort_unstable();
        visibilities.dedup();
        let mut models = response
            .models
            .into_iter()
            .filter(|model| model.visibility == "list" && (chatgpt_mode || model.supported_in_api))
            .collect::<Vec<_>>();
        models.sort_by_key(|model| model.priority);
        if models.is_empty() {
            anyhow::bail!(
                "the model catalog contains no selectable models \
                 ({catalog_count} entries, {api_supported} API-supported, visibility: {})",
                visibilities.join("/")
            );
        }
        return Ok(models);
    }
    if let Ok(response) = serde_json::from_str::<OpenAiModelsResponse>(&body) {
        let models = response
            .data
            .into_iter()
            .map(|model| ModelCatalogEntry::from_id(model.id))
            .collect::<Vec<_>>();
        if models.is_empty() {
            anyhow::bail!("the model catalog contains no models");
        }
        return Ok(models);
    }
    anyhow::bail!(
        "model catalog has an unsupported response shape: {}",
        compact_error(&body)
    )
}

fn is_event_stream(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
}

async fn read_sse(
    mut response: Response,
    idle_timeout: Duration,
    mut on_event: impl FnMut(&str) -> Result<()>,
) -> Result<String> {
    let mut decoder = SseDecoder::default();
    let mut body = Vec::new();
    while let Some(chunk) = next_response_chunk(&mut response, idle_timeout).await? {
        body.extend_from_slice(&chunk);
        for data in decoder.push(&chunk)? {
            on_event(&data)?;
        }
    }
    for data in decoder.finish()? {
        on_event(&data)?;
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

async fn read_response_body(mut response: Response, idle_timeout: Duration) -> Result<String> {
    let mut body = Vec::new();
    while let Some(chunk) = next_response_chunk(&mut response, idle_timeout).await? {
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

async fn next_response_chunk(
    response: &mut Response,
    idle_timeout: Duration,
) -> Result<Option<bytes::Bytes>> {
    match tokio::time::timeout(idle_timeout, response.chunk()).await {
        Ok(chunk) => chunk.context("cannot read model response stream"),
        Err(_) => anyhow::bail!(
            "model response stream received no data for {} seconds",
            idle_timeout.as_secs()
        ),
    }
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>> {
        self.buffer.extend_from_slice(chunk);
        self.take_events(false)
    }

    fn finish(&mut self) -> Result<Vec<String>> {
        self.take_events(true)
    }

    fn take_events(&mut self, finished: bool) -> Result<Vec<String>> {
        let mut events = Vec::new();
        while let Some((index, delimiter_len)) = sse_boundary(&self.buffer) {
            let frame = self.buffer.drain(..index).collect::<Vec<_>>();
            self.buffer.drain(..delimiter_len);
            if let Some(data) = sse_frame_data(&frame)? {
                events.push(data);
            }
        }
        if finished && !self.buffer.is_empty() {
            let frame = std::mem::take(&mut self.buffer);
            if let Some(data) = sse_frame_data(&frame)? {
                events.push(data);
            }
        }
        Ok(events)
    }
}

fn sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    for index in 0..buffer.len() {
        if buffer.get(index..index + 2) == Some(b"\n\n") {
            return Some((index, 2));
        }
        if buffer.get(index..index + 4) == Some(b"\r\n\r\n") {
            return Some((index, 4));
        }
    }
    None
}

fn sse_frame_data(frame: &[u8]) -> Result<Option<String>> {
    let frame = std::str::from_utf8(frame).context("response stream contained invalid UTF-8")?;
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>();
    Ok((!data.is_empty()).then(|| data.join("\n")))
}

#[derive(Default)]
struct ResponsesStream {
    text: String,
    calls: Vec<ToolCall>,
    completed_output: Option<Vec<Value>>,
}

impl ResponsesStream {
    fn push(&mut self, data: &str, on_text_delta: &mut impl FnMut(&str)) -> Result<()> {
        if data.trim().is_empty() || data.trim() == "[DONE]" {
            return Ok(());
        }
        let event: Value =
            serde_json::from_str(data).context("invalid event in ChatGPT response stream")?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.append_text(delta, on_text_delta);
                }
            }
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    self.collect_item(item, on_text_delta);
                }
            }
            Some("response.completed") => {
                self.completed_output = event
                    .pointer("/response/output")
                    .and_then(Value::as_array)
                    .cloned();
            }
            Some("response.failed" | "response.incomplete" | "error") => {
                let message = event
                    .pointer("/response/error/message")
                    .or_else(|| event.pointer("/error/message"))
                    .or_else(|| event.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("the ChatGPT response failed");
                anyhow::bail!("{message}");
            }
            _ => {}
        }
        Ok(())
    }

    fn append_text(&mut self, delta: &str, on_text_delta: &mut impl FnMut(&str)) {
        if delta.is_empty() {
            return;
        }
        self.text.push_str(delta);
        on_text_delta(delta);
    }

    fn collect_item(&mut self, item: &Value, on_text_delta: &mut impl FnMut(&str)) {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let item_text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<String>();
                if !item_text.is_empty() && !self.text.ends_with(&item_text) {
                    self.append_text(&item_text, on_text_delta);
                }
            }
            Some("function_call") => {
                let Some(id) = item.get("call_id").and_then(Value::as_str) else {
                    return;
                };
                let Some(name) = item.get("name").and_then(Value::as_str) else {
                    return;
                };
                self.calls.push(ToolCall {
                    id: id.to_owned(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: name.to_owned(),
                        arguments: item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}")
                            .to_owned(),
                    },
                });
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Result<Message> {
        if self.text.is_empty()
            && self.calls.is_empty()
            && let Some(items) = self.completed_output.take()
        {
            for item in items {
                self.collect_item(&item, &mut |_| {});
            }
        }
        completed_message(
            self.text,
            self.calls,
            "ChatGPT returned no message or tool call",
        )
    }
}

#[derive(Default)]
struct ChatToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct ChatCompletionStream {
    text: String,
    calls: Vec<ChatToolCall>,
}

impl ChatCompletionStream {
    fn push(&mut self, data: &str, on_text_delta: &mut impl FnMut(&str)) -> Result<()> {
        if data.trim().is_empty() || data.trim() == "[DONE]" {
            return Ok(());
        }
        let event: Value =
            serde_json::from_str(data).context("invalid Chat Completions stream event")?;
        if let Some(message) = event.pointer("/error/message").and_then(Value::as_str) {
            anyhow::bail!("{message}");
        }
        let Some(delta) = event.pointer("/choices/0/delta") else {
            return Ok(());
        };
        if let Some(content) = delta.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            self.text.push_str(content);
            on_text_delta(content);
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if self.calls.len() <= index {
                self.calls.resize_with(index + 1, ChatToolCall::default);
            }
            let target = &mut self.calls[index];
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                target.id.push_str(id);
            }
            if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                target.name.push_str(name);
            }
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                target.arguments.push_str(arguments);
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<Message> {
        let calls = self
            .calls
            .into_iter()
            .filter(|call| !call.id.is_empty() && !call.name.is_empty())
            .map(|call| ToolCall {
                id: call.id,
                kind: "function".into(),
                function: FunctionCall {
                    name: call.name,
                    arguments: if call.arguments.is_empty() {
                        "{}".into()
                    } else {
                        call.arguments
                    },
                },
            })
            .collect();
        completed_message(self.text, calls, "model returned no message or tool call")
    }
}

fn completed_message(text: String, calls: Vec<ToolCall>, empty_error: &str) -> Result<Message> {
    if text.is_empty() && calls.is_empty() {
        anyhow::bail!("{empty_error}");
    }
    Ok(Message {
        role: Role::Assistant,
        sequence: None,
        created_at: None,
        content: (!text.is_empty()).then_some(text),
        tool_calls: (!calls.is_empty()).then_some(calls),
        tool_call_id: None,
        hidden: false,
    })
}

fn compact_error(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.pointer("/detail/message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.chars().take(500).collect())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    #[test]
    fn chat_completions_payload_enables_parallel_tool_calls() {
        let request = ChatRequest {
            model: "gpt-test",
            messages: Vec::new(),
            tools: &[],
            tool_choice: "auto",
            parallel_tool_calls: true,
            stream: true,
            reasoning_effort: None,
            service_tier: None,
        };

        let payload = serde_json::to_value(request).unwrap();
        assert_eq!(payload["parallel_tool_calls"], true);
    }

    #[test]
    fn converts_chat_history_to_responses_tool_items() {
        let messages = vec![
            Message::text(Role::System, "system"),
            Message::text(Role::User, "inspect"),
            Message {
                role: Role::Assistant,
                sequence: None,
                created_at: None,
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "read_file".into(),
                        arguments: r#"{"path":"a.rs"}"#.into(),
                    },
                }]),
                tool_call_id: None,
                hidden: false,
            },
            Message {
                role: Role::Tool,
                sequence: None,
                created_at: None,
                content: Some("file contents".into()),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                hidden: false,
            },
        ];
        let payload = responses_payload(
            "gpt-test",
            Some("high"),
            Some("fast"),
            &messages,
            &[],
            Uuid::nil(),
        );
        assert_eq!(payload["instructions"], "system");
        assert_eq!(payload["input"][1]["type"], "function_call");
        assert_eq!(payload["input"][2]["type"], "function_call_output");
        assert_eq!(payload["parallel_tool_calls"], true);
        assert_eq!(payload["reasoning"]["effort"], "high");
        assert_eq!(payload["service_tier"], "fast");
    }

    #[test]
    fn parses_text_and_function_calls_from_sse() {
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"b.rs\\\"}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[]}}\n\n"
        );
        let mut deltas = Vec::new();
        let mut decoder = SseDecoder::default();
        let mut stream = ResponsesStream::default();
        for data in decoder.push(body.as_bytes()).unwrap() {
            stream
                .push(&data, &mut |delta| deltas.push(delta.to_owned()))
                .unwrap();
        }
        let message = stream.finish().unwrap();
        assert_eq!(message.content.as_deref(), Some("hello"));
        assert_eq!(deltas, ["hel", "lo"]);
        let calls = message.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[1].id, "call_2");
    }

    #[test]
    fn chat_completion_stream_reassembles_text_and_tool_call_deltas() {
        let mut stream = ChatCompletionStream::default();
        let mut deltas = Vec::new();
        for data in [
            r#"{"choices":[{"delta":{"role":"assistant","content":"hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_","arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"file","arguments":"\"a.rs\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ] {
            stream
                .push(data, &mut |delta| deltas.push(delta.to_owned()))
                .unwrap();
        }

        let message = stream.finish().unwrap();

        assert_eq!(message.content.as_deref(), Some("hello"));
        assert_eq!(deltas, ["hel", "lo"]);
        let call = &message.tool_calls.unwrap()[0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.function.name, "read_file");
        assert_eq!(call.function.arguments, r#"{"path":"a.rs"}"#);
    }

    #[tokio::test]
    async fn chat_completion_delivers_text_before_the_sse_finishes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16_384];
            let read = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let first = "data: {\"choices\":[{\"delta\":{\"content\":\"hello \"}}]}\n\n";
            socket
                .write_all(format!("{:X}\r\n{first}\r\n", first.len()).as_bytes())
                .await
                .unwrap();
            socket.flush().await.unwrap();
            release_rx.await.unwrap();
            let second = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"world\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            socket
                .write_all(format!("{:X}\r\n{second}\r\n", second.len()).as_bytes())
                .await
                .unwrap();
            socket.write_all(b"0\r\n\r\n").await.unwrap();
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        let config = Config {
            providers: std::collections::BTreeMap::from([(
                "openai".into(),
                crate::config::ProviderConfig::test(
                    "mock-model".into(),
                    format!("http://{address}/v1"),
                ),
            )]),
            request_timeout_seconds: 5,
            session_directories: Vec::new(),
            ..Config::default()
        };
        let provider = OpenAiCompatible::new(&config, &config.active_provider).unwrap();
        let (delta_tx, mut delta_rx) = tokio::sync::mpsc::unbounded_channel();
        let completion = tokio::spawn(async move {
            provider
                .complete(&[Message::text(Role::User, "Say hello")], &[], |delta| {
                    let _ = delta_tx.send(delta.to_owned());
                })
                .await
        });

        let first = tokio::time::timeout(Duration::from_secs(1), delta_rx.recv())
            .await
            .expect("first text delta was buffered until the stream ended")
            .unwrap();
        assert_eq!(first, "hello ");
        release_tx.send(()).unwrap();
        let message = completion.await.unwrap().unwrap();
        assert_eq!(message.content.as_deref(), Some("hello world"));
        let request = server.await.unwrap();
        assert!(request.contains("\"stream\":true"));
    }

    #[tokio::test]
    async fn stream_timeout_resets_after_each_received_chunk() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16_384];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            for content in [
                "{\"choices\":[{\"delta\":{\"content\":\"one \"}}]}",
                "{\"choices\":[{\"delta\":{\"content\":\"two \"}}]}",
                "{\"choices\":[{\"delta\":{\"content\":\"three\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]",
            ] {
                let event = format!("data: {content}\n\n");
                socket
                    .write_all(format!("{:X}\r\n{event}\r\n", event.len()).as_bytes())
                    .await
                    .unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
            socket.write_all(b"0\r\n\r\n").await.unwrap();
        });
        let config = Config {
            request_timeout_seconds: 1,
            ..Config::test("mock-model", format!("http://{address}/v1"))
        };
        let provider = OpenAiCompatible::new(&config, &config.active_provider).unwrap();

        let message = provider
            .complete(&[Message::text(Role::User, "Count")], &[], |_| {})
            .await
            .unwrap();

        assert_eq!(message.content.as_deref(), Some("one two three"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stream_times_out_only_after_a_full_idle_interval() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16_384];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let config = Config {
            request_timeout_seconds: 1,
            ..Config::test("mock-model", format!("http://{address}/v1"))
        };
        let provider = OpenAiCompatible::new(&config, &config.active_provider).unwrap();

        let error = match provider
            .complete(&[Message::text(Role::User, "Wait")], &[], |_| {})
            .await
        {
            Ok(_) => panic!("idle stream unexpectedly completed"),
            Err(error) => error,
        };

        assert!(
            format!("{error:#}").contains("received no data for 1 seconds"),
            "{error:#}"
        );
        server.abort();
    }

    #[test]
    fn manual_catalog_adds_merges_and_filters_models() {
        let remote = ModelCatalogEntry {
            display_name: "Remote name".into(),
            supported_reasoning_levels: vec![ReasoningOption {
                effort: "low".into(),
                name: String::new(),
                description: "Remote low".into(),
            }],
            input_modalities: vec!["text".into()],
            ..ModelCatalogEntry::from_id("remote-model".into())
        };
        let configured = BTreeMap::from([
            (
                "remote-model".into(),
                ModelCapabilitiesConfig {
                    display_name: Some("Configured name".into()),
                    reasoning_levels: vec![CatalogOptionConfig {
                        id: "high".into(),
                        name: Some("Deep".into()),
                        description: None,
                    }],
                    input_modalities: vec!["image".into()],
                    ..ModelCapabilitiesConfig::default()
                },
            ),
            (
                "manual-model".into(),
                ModelCapabilitiesConfig {
                    reasoning_levels: vec![CatalogOptionConfig {
                        id: "medium".into(),
                        name: None,
                        description: None,
                    }],
                    service_tiers: vec![CatalogOptionConfig {
                        id: "priority".into(),
                        name: None,
                        description: None,
                    }],
                    ..ModelCapabilitiesConfig::default()
                },
            ),
        ]);

        let models = merge_model_catalog(
            vec![remote],
            &configured,
            Some(&["manual-model".into(), "remote-model".into()]),
        )
        .unwrap();

        assert_eq!(models[0].slug, "manual-model");
        assert_eq!(models[0].supported_reasoning_levels[0].name, "medium");
        assert_eq!(models[0].service_tiers[0].name, "priority");
        assert_eq!(models[1].display_name, "Configured name");
        assert_eq!(models[1].supported_reasoning_levels[0].name, "low");
        assert_eq!(models[1].supported_reasoning_levels[1].name, "Deep");
        assert_eq!(models[1].input_modalities, ["text", "image"]);
    }

    #[test]
    fn configured_default_must_exist_after_merge() {
        let configured = BTreeMap::from([(
            "manual".into(),
            ModelCapabilitiesConfig {
                default_reasoning_level: Some("missing".into()),
                ..ModelCapabilitiesConfig::default()
            },
        )]);

        let result = merge_model_catalog(Vec::new(), &configured, None);

        assert!(format!("{:#}", result.err().unwrap()).contains("merged catalog"));
    }

    #[test]
    fn allowed_models_rejects_unknown_model() {
        let result = merge_model_catalog(
            vec![ModelCatalogEntry::from_id("known".into())],
            &BTreeMap::new(),
            Some(&["missing".into()]),
        );

        assert!(format!("{:#}", result.err().unwrap()).contains("missing"));
    }

    #[test]
    fn selected_model_must_remain_in_allowed_models() {
        let mut config = Config::test("auto", "http://127.0.0.1:1/v1");
        config.providers.get_mut("openai").unwrap().allowed_models = Some(vec!["allowed".into()]);
        let mut provider = OpenAiCompatible::new(&config, "openai").unwrap();
        provider.set_selection(&ModelSelection {
            model: "old-session-model".into(),
            reasoning_effort: None,
            service_tier: None,
        });

        assert!(
            format!("{:#}", provider.selected_model_is_allowed().unwrap_err())
                .contains("allowed_models")
        );
    }

    #[tokio::test]
    async fn disabled_models_endpoint_uses_only_manual_catalog() {
        let mut config = Config::test("auto", "http://127.0.0.1:1/v1");
        let provider = config.providers.get_mut("openai").unwrap();
        provider.fetch_models = false;
        provider
            .model_capabilities
            .insert("manual-model".into(), ModelCapabilitiesConfig::default());
        let provider = OpenAiCompatible::new(&config, "openai").unwrap();

        let models = provider.fetch_models().await.unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "manual-model");
    }

    #[test]
    fn default_service_tier_is_not_shown_as_a_separate_speed() {
        let model = ModelCatalogEntry {
            service_tiers: vec![
                ServiceTierOption {
                    id: "default".into(),
                    name: "Standard".into(),
                    description: "Normal speed".into(),
                },
                ServiceTierOption {
                    id: "priority".into(),
                    name: "Fast".into(),
                    description: "Faster responses".into(),
                },
            ],
            ..ModelCatalogEntry::from_id("test-model".into())
        };

        let tiers = model.available_service_tiers();

        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].id, "priority");
        assert_eq!(tiers[0].name, "Fast");
    }

    #[test]
    fn codecrab_default_selects_sol_high_and_catalog_fast_id() {
        let catalog = vec![
            ModelCatalogEntry::from_id("another-model".into()),
            ModelCatalogEntry {
                slug: "gpt-5.6-sol".into(),
                display_name: "GPT-5.6-Sol".into(),
                default_reasoning_level: Some("low".into()),
                supported_reasoning_levels: vec![
                    ReasoningOption {
                        effort: "low".into(),
                        name: "low".into(),
                        description: "Quick".into(),
                    },
                    ReasoningOption {
                        effort: "high".into(),
                        name: "high".into(),
                        description: "Deep".into(),
                    },
                ],
                service_tiers: vec![ServiceTierOption {
                    id: "provider-fast-id".into(),
                    name: "Fast".into(),
                    description: "Faster".into(),
                }],
                ..ModelCatalogEntry::from_id("gpt-5.6-sol".into())
            },
        ];

        let selection = default_model_selection(&catalog).unwrap();

        assert_eq!(selection.model, "gpt-5.6-sol");
        assert_eq!(selection.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(selection.service_tier.as_deref(), Some("provider-fast-id"));
    }

    #[tokio::test]
    async fn fetches_the_provider_catalog_and_parses_capabilities() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = json!({
            "models": [{
                "slug": "future-model-variant",
                "display_name": "Future Model - Variant",
                "description": "A catalog-provided model",
                "default_reasoning_level": "high",
                "supported_reasoning_levels": [
                    {"effort": "low", "description": "Faster"},
                    {"effort": "high", "description": "Deeper"}
                ],
                "visibility": "list",
                "supported_in_api": true,
                "priority": 10,
                "service_tiers": [{
                    "id": "accelerated",
                    "name": "Accelerated",
                    "description": "Catalog-provided speed"
                }],
                "default_service_tier": "default"
            }]
        })
        .to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16_384];
            let read = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        let config = Config {
            providers: std::collections::BTreeMap::from([(
                "openai".into(),
                crate::config::ProviderConfig::test("auto".into(), format!("http://{address}/v1")),
            )]),
            ..Config::default()
        };
        let provider = OpenAiCompatible::new(&config, &config.active_provider).unwrap();

        let models = provider.fetch_models().await.unwrap();

        assert_eq!(models[0].slug, "future-model-variant");
        assert_eq!(models[0].supported_reasoning_levels[1].effort, "high");
        assert_eq!(models[0].service_tiers[0].id, "accelerated");
        let request = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(request.starts_with("GET /v1/models?client_version="));
    }

    #[tokio::test]
    #[ignore = "requires an interactive ChatGPT OAuth login and network access"]
    async fn fetches_the_live_chatgpt_catalog() {
        let provider =
            OpenAiCompatible::new(&Config::default(), crate::config::DEFAULT_PROVIDER).unwrap();
        let models = provider.fetch_models().await.unwrap();
        assert!(!models.is_empty());
        assert!(
            models
                .iter()
                .any(|model| !model.supported_reasoning_levels.is_empty())
        );
        assert!(
            models
                .iter()
                .any(|model| !model.available_service_tiers().is_empty())
        );
    }
}
