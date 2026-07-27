use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth::{OAuthCredentials, OAuthStore},
    config::Config,
};

const CHATGPT_CODEX_BASE: &str = "https://chatgpt.com/backend-api/codex";
const CHATGPT_CODEX_RESPONSES: &str = "https://chatgpt.com/backend-api/codex/responses";
// This is a Codex protocol compatibility version, not CodeCrab's package
// version. The neutral value requests the account's compatible catalog.
const CODEX_CATALOG_COMPAT_VERSION: &str = "0.0.0";

#[derive(Deserialize)]
pub(crate) struct ReasoningOption {
    pub effort: String,
    pub description: String,
}

#[derive(Clone, Deserialize)]
pub(crate) struct ServiceTierOption {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[derive(Deserialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub(crate) fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
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
    messages: &'a [Message],
    tools: &'a [Value],
    tool_choice: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<&'a str>,
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
    model: String,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    session_id: Uuid,
    pub max_tool_rounds: usize,
}

impl OpenAiCompatible {
    pub(crate) fn new(config: &Config) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_seconds))
            .build()?;
        let official_openai = config.base_url.trim_end_matches('/') == "https://api.openai.com/v1";
        let auth_mode = config.auth.trim().to_ascii_lowercase();
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
            "api_key" => false,
            other => anyhow::bail!("invalid auth mode {other:?}; expected auto, oauth, or api_key"),
        };

        let backend = if use_oauth {
            Backend::ChatGptSubscription { auth: oauth }
        } else {
            let api_key = config.api_key().with_context(|| {
                if official_openai {
                    "not authenticated; run `codecrab auth login` to use ChatGPT Pro, or set OPENAI_API_KEY"
                } else {
                    "provider API credential is unavailable"
                }
            })?;
            Backend::ChatCompletions {
                base_url: config.base_url.trim_end_matches('/').to_owned(),
                api_key,
            }
        };

        Ok(Self {
            client,
            backend,
            model: config.model.clone(),
            reasoning_effort: None,
            service_tier: None,
            session_id: Uuid::new_v4(),
            max_tool_rounds: config.max_tool_rounds,
        })
    }

    pub(crate) fn set_selection(&mut self, selection: &ModelSelection) {
        self.model.clone_from(&selection.model);
        self.reasoning_effort
            .clone_from(&selection.reasoning_effort);
        self.service_tier.clone_from(&selection.service_tier);
    }

    pub(crate) async fn fetch_models(&self) -> Result<Vec<ModelCatalogEntry>> {
        match &self.backend {
            Backend::ChatCompletions {
                base_url, api_key, ..
            } => {
                let url = format!("{base_url}/models");
                let response = self
                    .send_models_request(&url, api_key.as_deref(), None)
                    .await?;
                parse_models_response(response, false).await
            }
            Backend::ChatGptSubscription { auth } => {
                let url = format!("{CHATGPT_CODEX_BASE}/models");
                let credentials = auth.credentials().await?;
                let mut response = self
                    .send_models_request(&url, Some(&credentials.access_token), Some(&credentials))
                    .await?;
                if response.status() == StatusCode::UNAUTHORIZED {
                    let credentials = auth.refresh_credentials().await?;
                    response = self
                        .send_models_request(
                            &url,
                            Some(&credentials.access_token),
                            Some(&credentials),
                        )
                        .await?;
                }
                parse_models_response(response, true).await
            }
        }
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
        request.send().await.context("model catalog request failed")
    }

    pub(crate) async fn complete(&self, messages: &[Message], tools: &[Value]) -> Result<Message> {
        match &self.backend {
            Backend::ChatCompletions { base_url, api_key } => {
                self.complete_chat(base_url, api_key.as_deref(), messages, tools)
                    .await
            }
            Backend::ChatGptSubscription { auth } => {
                self.complete_subscription(auth, messages, tools).await
            }
        }
    }

    async fn complete_chat(
        &self,
        base_url: &str,
        api_key: Option<&str>,
        messages: &[Message],
        tools: &[Value],
    ) -> Result<Message> {
        let mut request = self
            .client
            .post(format!("{base_url}/chat/completions"))
            .json(&ChatRequest {
                model: &self.model,
                messages,
                tools,
                tool_choice: "auto",
                reasoning_effort: self.reasoning_effort.as_deref(),
                service_tier: self.service_tier.as_deref(),
            });
        if let Some(api_key) = api_key {
            request = request.bearer_auth(api_key);
        }
        let response = request.send().await.context("model request failed")?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            anyhow::bail!("model returned {status}: {}", compact_error(&body));
        }
        let parsed: ChatResponse =
            serde_json::from_str(&body).context("model returned an invalid response")?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message)
            .context("model returned no choices")
    }

    async fn complete_subscription(
        &self,
        auth: &OAuthStore,
        messages: &[Message],
        tools: &[Value],
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
            let credentials = auth.refresh_credentials().await?;
            response = self.send_subscription(&credentials, &payload).await?;
        }
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            anyhow::bail!(
                "ChatGPT subscription returned {status}: {}",
                compact_error(&body)
            );
        }
        parse_responses_stream(&body)
    }

    async fn send_subscription(
        &self,
        credentials: &OAuthCredentials,
        payload: &Value,
    ) -> Result<Response> {
        self.client
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
            .send()
            .await
            .context("ChatGPT subscription request failed")
    }
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
        "parallel_tool_calls": false,
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

async fn parse_models_response(
    response: Response,
    chatgpt_mode: bool,
) -> Result<Vec<ModelCatalogEntry>> {
    let status = response.status();
    let body = response.text().await?;
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

fn parse_responses_stream(body: &str) -> Result<Message> {
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut completed_output: Option<Vec<Value>> = None;

    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let event: Value =
            serde_json::from_str(data).context("invalid event in ChatGPT response stream")?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    collect_response_item(item, &mut text, &mut calls);
                }
            }
            Some("response.completed") => {
                completed_output = event
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
    }

    if text.is_empty()
        && calls.is_empty()
        && let Some(items) = completed_output
    {
        for item in items {
            collect_response_item(&item, &mut text, &mut calls);
        }
    }
    if text.is_empty() && calls.is_empty() {
        anyhow::bail!("ChatGPT returned no message or tool call");
    }
    Ok(Message {
        role: Role::Assistant,
        content: (!text.is_empty()).then_some(text),
        tool_calls: (!calls.is_empty()).then_some(calls),
        tool_call_id: None,
    })
}

fn collect_response_item(item: &Value, text: &mut String, calls: &mut Vec<ToolCall>) {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if let Some(value) = part.get("text").and_then(Value::as_str) {
                        text.push_str(value);
                    }
                }
            }
        }
        Some("function_call") => {
            let Some(id) = item.get("call_id").and_then(Value::as_str) else {
                return;
            };
            let Some(name) = item.get("name").and_then(Value::as_str) else {
                return;
            };
            calls.push(ToolCall {
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
    fn converts_chat_history_to_responses_tool_items() {
        let messages = vec![
            Message::text(Role::System, "system"),
            Message::text(Role::User, "inspect"),
            Message {
                role: Role::Assistant,
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
            },
            Message {
                role: Role::Tool,
                content: Some("file contents".into()),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
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
        assert_eq!(payload["reasoning"]["effort"], "high");
        assert_eq!(payload["service_tier"], "fast");
    }

    #[test]
    fn parses_text_and_function_calls_from_sse() {
        let body = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[]}}\n\n"
        );
        let message = parse_responses_stream(body).unwrap();
        assert_eq!(message.content.as_deref(), Some("hello"));
        assert_eq!(
            message.tool_calls.as_ref().unwrap()[0].function.name,
            "read_file"
        );
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
            base_url: format!("http://{address}/v1"),
            auth: "api_key".into(),
            api_key_env: String::new(),
            ..Config::default()
        };
        let provider = OpenAiCompatible::new(&config).unwrap();

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
        let provider = OpenAiCompatible::new(&Config::default()).unwrap();
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
