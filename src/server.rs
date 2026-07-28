use std::{
    convert::Infallible,
    future::{Future, IntoFuture},
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::DefaultBodyLimit,
    extract::State,
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc, oneshot, watch},
};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::{
    agent::{Agent, turn_was_cancelled},
    auth::OAuthStore,
    completion::{CompletionItem, complete as complete_input},
    config::{
        Config, ConfigStore, ProviderConfig, ProviderSummary, SessionRegistry, paths_equal,
        validate_provider_name,
    },
    events::{AgentActivity, AgentEvent},
    provider::{
        Message, ModelCatalogEntry, ModelSelection, OpenAiCompatible, default_model_selection,
    },
    session::{
        Session, SessionProject, SessionStore, list_session_projects, resolve_global_session,
    },
    skills::SkillRegistry,
    tools::ToolBox,
    transcription::Transcriber,
};

const INDEX_HTML: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/index.html"));
const APP_JS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/app.js"));
const APP_CSS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/app.css"));
const SHUTDOWN_WARNING_DELAY: Duration = Duration::from_millis(100);
const SHUTDOWN_WAITING_MESSAGE: &str = "CodeCrab is still shutting down because active HTTP \
requests or open connections have not finished. Press Ctrl+C again to force exit.";

#[derive(Debug, Eq, PartialEq)]
enum ShutdownOutcome {
    Graceful,
    Forced,
}

#[derive(Clone)]
struct ServerState {
    inner: Arc<ServerInner>,
}

struct ServerInner {
    config: RwLock<Config>,
    registry: SessionRegistry,
    debug_openai: bool,
    models: RwLock<Vec<ModelCatalogEntry>>,
    catalog_error: RwLock<Option<String>>,
    dictation_available: bool,
    workspace: Mutex<ServerWorkspace>,
    active_turn: Mutex<Option<watch::Sender<bool>>>,
}

struct ServerWorkspace {
    root: PathBuf,
    agent: Option<Agent>,
}

#[derive(Serialize)]
struct StateResponse {
    project: String,
    session: Option<Session>,
    projects: Vec<SessionProject>,
    skills: Vec<SkillResponse>,
    models: Vec<ModelCatalogEntry>,
    catalog_error: Option<String>,
    dictation_available: bool,
    providers: Vec<ProviderSummary>,
}

#[derive(Serialize)]
struct SkillResponse {
    name: String,
    description: String,
    scope: &'static str,
}

#[derive(Deserialize)]
struct ChatRequest {
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    continuation: bool,
}

#[derive(Deserialize)]
struct ModelRequest {
    model: String,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
}

#[derive(Deserialize)]
struct SessionRequest {
    project: Option<PathBuf>,
    id: String,
}

#[derive(Deserialize)]
struct CompletionRequest {
    before_cursor: String,
    after_cursor: String,
}

#[derive(Deserialize)]
struct GoalRequest {
    id: Option<Uuid>,
    objective: Option<String>,
}

#[derive(Deserialize)]
struct ProviderRequest {
    name: String,
    model: Option<String>,
    base_url: Option<String>,
    auth: Option<String>,
    api_key: Option<String>,
    #[serde(default)]
    clear_api_key: bool,
}

#[derive(Serialize)]
struct CompletionResponse {
    items: Vec<CompletionItem>,
    replace_before: String,
    replace_after: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatStreamMessage {
    UserMessage {
        message: Message,
    },
    AssistantMessage {
        message: Message,
    },
    AssistantTextDelta {
        delta: String,
        start: bool,
        sequence: u64,
        created_at: chrono::DateTime<chrono::Utc>,
    },
    AssistantStreamReset,
    AssistantMessageCompleted {
        message: Message,
    },
    Activity {
        activity: AgentActivity,
    },
    Done {
        state: StateResponse,
    },
    Cancelled {
        state: StateResponse,
    },
    Error {
        error: String,
    },
}

pub(crate) async fn serve(
    root: PathBuf,
    config: Config,
    host: String,
    port: u16,
    debug_openai: bool,
) -> Result<()> {
    let store = SessionStore::new(&root)?;
    let registry = SessionRegistry::global();
    let dictation_available = OAuthStore::new()?.is_logged_in();
    let active = config.provider(&config.active_provider)?;
    let session =
        store.create_for_provider(config.active_provider.clone(), active.model.clone())?;
    let mut agent = build_agent(&root, &config, debug_openai, session)?;
    let (models, catalog_error) = match agent.fetch_models().await {
        Ok(models) => {
            agent.resolve_auto_model(&models);
            (models, None)
        }
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    list_session_projects(&root, &registry)?;

    let state = ServerState {
        inner: Arc::new(ServerInner {
            config: RwLock::new(config),
            registry,
            debug_openai,
            models: RwLock::new(models),
            catalog_error: RwLock::new(catalog_error),
            dictation_available,
            workspace: Mutex::new(ServerWorkspace {
                root,
                agent: Some(agent),
            }),
            active_turn: Mutex::new(None),
        }),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(javascript))
        .route("/app.css", get(stylesheet))
        .nest(
            "/api",
            Router::new()
                .route("/health", get(health))
                .route("/state", get(get_state))
                .route("/completions", post(completions))
                .route("/chat", post(chat))
                .route("/chat/cancel", post(cancel_chat))
                .route(
                    "/transcribe",
                    post(transcribe).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
                )
                .route("/model", put(set_model))
                .route("/providers", post(save_provider))
                .route("/providers/use", post(use_provider))
                .route("/providers/delete", post(delete_provider))
                .route("/session/clear", post(clear_session))
                .route("/sessions", post(new_session))
                .route("/sessions/delete", post(delete_session))
                .route("/sessions/resume", post(resume_session))
                .route("/goals/create", post(create_goal))
                .route("/goals/edit", put(edit_goal))
                .route("/goals/activate", post(activate_goal))
                .route("/goals/pause", post(pause_goal))
                .route("/goals/delete", post(delete_goal))
                .fallback(api_not_found),
        )
        .fallback(index)
        .with_state(state);

    let listener = TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| format!("cannot bind web server to {host}:{port}"))?;
    let address = listener.local_addr()?;
    let display_host = match address.ip() {
        ip if ip.is_unspecified() => "127.0.0.1".to_owned(),
        ip => ip.to_string(),
    };
    let origin = format!("http://{display_host}:{}", address.port());
    println!("API: {origin}/api");
    println!("Web: {origin}/");

    let outcome = serve_until_shutdown(listener, app, shutdown_signal(), shutdown_signal()).await?;
    if outcome == ShutdownOutcome::Forced {
        eprintln!("Forcing CodeCrab to exit immediately.");
        std::process::exit(130);
    }
    Ok(())
}

fn build_agent(
    root: &std::path::Path,
    config: &Config,
    debug_openai: bool,
    session: Session,
) -> Result<Agent> {
    let mut provider = OpenAiCompatible::new(config, &session.provider)?;
    provider.set_debug_openai(debug_openai);
    let tools = ToolBox::new(root.to_path_buf());
    Agent::new(provider, tools, SkillRegistry::discover(root), session)
}

async fn refresh_catalog(inner: &ServerInner, agent: &mut Agent) {
    match agent.fetch_models().await {
        Ok(models) => {
            agent.resolve_auto_model(&models);
            *inner.models.write().unwrap() = models;
            *inner.catalog_error.write().unwrap() = None;
        }
        Err(error) => {
            *inner.models.write().unwrap() = Vec::new();
            *inner.catalog_error.write().unwrap() = Some(format!("{error:#}"));
        }
    }
}

fn save_agent_session(inner: &ServerInner, agent: &Agent) -> Result<()> {
    let root = agent.project_root();
    SessionStore::new(root)?.save(agent.session())?;
    inner.registry.register(root)
}

fn resolve_session_root(
    current_root: &std::path::Path,
    registry: &SessionRegistry,
    request: &SessionRequest,
) -> Result<PathBuf> {
    if let Some(project) = &request.project {
        return Ok(project.clone());
    }
    let projects = list_session_projects(current_root, registry)?;
    resolve_global_session(&projects, Some(&request.id)).map(|(root, _)| root)
}

async fn snapshot(state: &ServerState) -> Result<StateResponse> {
    let workspace = state.inner.workspace.lock().await;
    let session = workspace
        .agent
        .as_ref()
        .map(|agent| agent.session().clone());
    let root = workspace.root.clone();
    let skill_registry = workspace
        .agent
        .is_none()
        .then(|| SkillRegistry::discover(&root));
    let skills = workspace
        .agent
        .as_ref()
        .map(Agent::skills)
        .unwrap_or_else(|| skill_registry.as_ref().unwrap().skills())
        .iter()
        .map(|skill| SkillResponse {
            name: skill.name.clone(),
            description: skill.description.clone(),
            scope: skill.scope.label(),
        })
        .collect();
    drop(workspace);
    let projects = list_session_projects(&root, &state.inner.registry)?;
    let project = projects
        .first()
        .map(|project| project.root.display().to_string())
        .unwrap_or_else(|| root.display().to_string());
    let providers = state.inner.config.read().unwrap().summaries();
    Ok(StateResponse {
        project,
        session,
        projects,
        skills,
        models: state.inner.models.read().unwrap().clone(),
        catalog_error: state.inner.catalog_error.read().unwrap().clone(),
        dictation_available: state.inner.dictation_available,
        providers,
    })
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION")
    }))
}

async fn get_state(State(state): State<ServerState>) -> ApiResult<StateResponse> {
    let workspace = state.inner.workspace.lock().await;
    if let Some(agent) = &workspace.agent {
        save_agent_session(&state.inner, agent)?;
    }
    drop(workspace);
    Ok(Json(snapshot(&state).await?))
}

async fn completions(
    State(state): State<ServerState>,
    Json(request): Json<CompletionRequest>,
) -> ApiResult<Option<CompletionResponse>> {
    let cursor = request.before_cursor.len();
    let input = format!("{}{}", request.before_cursor, request.after_cursor);
    let workspace = state.inner.workspace.lock().await;
    let Some(agent) = &workspace.agent else {
        return Err(ApiError::message(
            StatusCode::CONFLICT,
            "create or resume a session before requesting completions",
        ));
    };
    let root = agent.project_root();
    let Some(menu) = complete_input(
        &input,
        cursor,
        root,
        agent
            .skills()
            .iter()
            .map(|skill| (skill.name.as_str(), skill.description.as_str())),
    ) else {
        return Ok(Json(None));
    };
    Ok(Json(Some(CompletionResponse {
        replace_before: input[menu.token_start..cursor].to_owned(),
        replace_after: input[cursor..menu.token_end].to_owned(),
        items: menu.items,
    })))
}

#[derive(Serialize)]
struct TranscriptResponse {
    text: String,
}

async fn transcribe(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<TranscriptResponse> {
    if body.is_empty() {
        return Err(ApiError::message(
            StatusCode::BAD_REQUEST,
            "recording is empty",
        ));
    }
    if !state.inner.dictation_available {
        return Err(ApiError::message(
            StatusCode::UNAUTHORIZED,
            "voice dictation requires ChatGPT OAuth; run `codecrab auth login`",
        ));
    }
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.starts_with("audio/"))
        .unwrap_or("audio/webm");
    let text = Transcriber::new(state.inner.debug_openai)?
        .transcribe(body.to_vec(), content_type)
        .await?;
    Ok(Json(TranscriptResponse { text }))
}

async fn chat(
    State(state): State<ServerState>,
    Json(request): Json<ChatRequest>,
) -> std::result::Result<Response, ApiError> {
    let prompt = request.prompt.trim().to_owned();
    if !request.continuation && prompt.is_empty() {
        return Err(ApiError::message(
            StatusCode::BAD_REQUEST,
            "prompt is empty",
        ));
    }
    let (cancel_tx, cancellation) = watch::channel(false);
    {
        let mut active_turn = state.inner.active_turn.lock().await;
        if active_turn.is_some() {
            return Err(ApiError::message(
                StatusCode::CONFLICT,
                "an agent turn is already running",
            ));
        }
        *active_turn = Some(cancel_tx);
    }

    let (output_tx, output_rx) = mpsc::channel::<std::result::Result<Bytes, Infallible>>(32);
    tokio::spawn(async move {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let event_output = output_tx.clone();
        let forward_events = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let message = match event {
                    AgentEvent::UserMessage(message) => ChatStreamMessage::UserMessage { message },
                    AgentEvent::AssistantMessage(message) => {
                        ChatStreamMessage::AssistantMessage { message }
                    }
                    AgentEvent::AssistantTextDelta {
                        delta,
                        start,
                        sequence,
                        created_at,
                    } => ChatStreamMessage::AssistantTextDelta {
                        delta,
                        start,
                        sequence,
                        created_at,
                    },
                    AgentEvent::AssistantStreamReset => ChatStreamMessage::AssistantStreamReset,
                    AgentEvent::AssistantMessageCompleted(message) => {
                        ChatStreamMessage::AssistantMessageCompleted { message }
                    }
                    AgentEvent::Activity(activity) => ChatStreamMessage::Activity { activity },
                };
                if !send_stream_message(&event_output, message).await {
                    break;
                }
            }
        });

        let mut workspace = state.inner.workspace.lock().await;
        let (result, save_result) = if let Some(agent) = workspace.agent.as_mut() {
            let result = if request.continuation {
                agent
                    .continue_goal_with_events(event_tx, cancellation)
                    .await
            } else {
                agent
                    .turn_with_events(&prompt, event_tx, cancellation)
                    .await
            };
            let save_result = save_agent_session(&state.inner, agent);
            (result, save_result)
        } else {
            (
                Err(anyhow::anyhow!(
                    "create or resume a session before sending a message"
                )),
                Ok(()),
            )
        };
        drop(workspace);
        let _ = forward_events.await;
        *state.inner.active_turn.lock().await = None;

        let result = result.and(save_result.map(|_| ()));
        let message = match result {
            Ok(()) => match snapshot(&state).await {
                Ok(state) => ChatStreamMessage::Done { state },
                Err(error) => stream_error(error),
            },
            Err(error) if turn_was_cancelled(&error) => match snapshot(&state).await {
                Ok(state) => ChatStreamMessage::Cancelled { state },
                Err(error) => stream_error(error),
            },
            Err(error) => stream_error(error),
        };
        let _ = send_stream_message(&output_tx, message).await;
    });

    let mut response = Body::from_stream(ReceiverStream::new(output_rx)).into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache"),
    );
    Ok(response)
}

fn stream_error(error: anyhow::Error) -> ChatStreamMessage {
    let error = format!("{error:#}");
    eprintln!("CodeCrab agent turn failed: {error}");
    ChatStreamMessage::Error { error }
}

async fn cancel_chat(State(state): State<ServerState>) -> Json<serde_json::Value> {
    let cancelled = state
        .inner
        .active_turn
        .lock()
        .await
        .as_ref()
        .is_some_and(|cancel| cancel.send(true).is_ok());
    Json(json!({ "cancelled": cancelled }))
}

async fn send_stream_message(
    output: &mpsc::Sender<std::result::Result<Bytes, Infallible>>,
    message: ChatStreamMessage,
) -> bool {
    let mut line = match serde_json::to_vec(&message) {
        Ok(line) => line,
        Err(_) => return false,
    };
    line.push(b'\n');
    output.send(Ok(Bytes::from(line))).await.is_ok()
}

async fn set_model(
    State(state): State<ServerState>,
    Json(request): Json<ModelRequest>,
) -> ApiResult<StateResponse> {
    if let Some(model) = state
        .inner
        .models
        .read()
        .unwrap()
        .iter()
        .find(|model| model.slug == request.model)
    {
        if let Some(reasoning) = &request.reasoning_effort
            && !model
                .supported_reasoning_levels
                .iter()
                .any(|option| option.effort == *reasoning)
        {
            return Err(ApiError::message(
                StatusCode::BAD_REQUEST,
                "reasoning effort is not supported by this model",
            ));
        }
        if let Some(tier) = &request.service_tier
            && !model.service_tiers.iter().any(|option| option.id == *tier)
        {
            return Err(ApiError::message(
                StatusCode::BAD_REQUEST,
                "service tier is not supported by this model",
            ));
        }
    } else if !state.inner.models.read().unwrap().is_empty() {
        return Err(ApiError::message(
            StatusCode::BAD_REQUEST,
            "model is not in the provider catalog",
        ));
    }

    let mut workspace = state.inner.workspace.lock().await;
    let Some(agent) = workspace.agent.as_mut() else {
        return Err(ApiError::message(
            StatusCode::CONFLICT,
            "create or resume a session before changing the model",
        ));
    };
    agent.set_model_selection(ModelSelection {
        model: request.model,
        reasoning_effort: request.reasoning_effort,
        service_tier: request.service_tier,
    });
    save_agent_session(&state.inner, agent)?;
    drop(workspace);
    Ok(Json(snapshot(&state).await?))
}

async fn save_provider(
    State(state): State<ServerState>,
    Json(request): Json<ProviderRequest>,
) -> ApiResult<StateResponse> {
    validate_provider_name(&request.name)?;
    {
        let store = ConfigStore::global()?;
        let mut config = store.load()?;
        let existing = config.providers.get(&request.name);
        let api_key = if request.clear_api_key {
            String::new()
        } else {
            request
                .api_key
                .filter(|key| !key.is_empty())
                .or_else(|| existing.map(|provider| provider.api_key.clone()))
                .unwrap_or_default()
        };
        let provider = ProviderConfig {
            model: request.model.unwrap_or_else(|| "auto".into()),
            base_url: request.base_url.context("base_url is required")?,
            auth: request.auth.unwrap_or_else(|| "api_key".into()),
            api_key,
            ..existing.cloned().unwrap_or_default()
        };
        provider.validate(&request.name)?;
        config.providers.insert(request.name, provider);
        store.save(&config)?;
        *state.inner.config.write().unwrap() = config;
    }
    Ok(Json(snapshot(&state).await?))
}

async fn use_provider(
    State(state): State<ServerState>,
    Json(request): Json<ProviderRequest>,
) -> ApiResult<StateResponse> {
    {
        let store = ConfigStore::global()?;
        let mut config = store.load()?;
        config.provider(&request.name)?;
        config.active_provider = request.name;
        store.save(&config)?;
        *state.inner.config.write().unwrap() = config;
    }
    Ok(Json(snapshot(&state).await?))
}

async fn delete_provider(
    State(state): State<ServerState>,
    Json(request): Json<ProviderRequest>,
) -> ApiResult<StateResponse> {
    {
        let store = ConfigStore::global()?;
        let mut config = store.load()?;
        config.provider(&request.name)?;
        if config.active_provider == request.name {
            return Err(ApiError::message(
                StatusCode::CONFLICT,
                "select another active provider before deleting this one",
            ));
        }
        config.providers.remove(&request.name);
        store.save(&config)?;
        *state.inner.config.write().unwrap() = config;
    }
    Ok(Json(snapshot(&state).await?))
}

async fn clear_session(State(state): State<ServerState>) -> ApiResult<StateResponse> {
    let mut workspace = state.inner.workspace.lock().await;
    let Some(agent) = workspace.agent.as_mut() else {
        return Err(ApiError::message(
            StatusCode::CONFLICT,
            "create or resume a session before clearing it",
        ));
    };
    agent.clear();
    save_agent_session(&state.inner, agent)?;
    drop(workspace);
    Ok(Json(snapshot(&state).await?))
}

async fn new_session(State(state): State<ServerState>) -> ApiResult<StateResponse> {
    let root = state.inner.workspace.lock().await.root.clone();
    let session = configured_new_session(&state, &root)?;
    let mut agent = build_agent(
        &root,
        &state.inner.config.read().unwrap(),
        state.inner.debug_openai,
        session,
    )?;
    refresh_catalog(&state.inner, &mut agent).await;
    save_agent_session(&state.inner, &agent)?;
    state.inner.workspace.lock().await.agent = Some(agent);
    Ok(Json(snapshot(&state).await?))
}

fn configured_new_session(state: &ServerState, root: &std::path::Path) -> Result<Session> {
    let config = state.inner.config.read().unwrap();
    let active = config.provider(&config.active_provider)?;
    let mut session = SessionStore::new(root)?
        .create_for_provider(config.active_provider.clone(), active.model.clone())?;
    if session.model == "auto"
        && let Some(selection) = default_model_selection(&state.inner.models.read().unwrap())
    {
        session.model = selection.model;
        session.reasoning_effort = selection.reasoning_effort;
        session.service_tier = selection.service_tier;
    }
    Ok(session)
}

async fn resume_session(
    State(state): State<ServerState>,
    Json(request): Json<SessionRequest>,
) -> ApiResult<StateResponse> {
    let current_root = state.inner.workspace.lock().await.root.clone();
    let root = resolve_session_root(&current_root, &state.inner.registry, &request)?;
    let store = SessionStore::new(&root)?;
    let session = store.load(Some(&request.id))?;
    let mut agent = build_agent(
        &root,
        &state.inner.config.read().unwrap(),
        state.inner.debug_openai,
        session,
    )?;
    refresh_catalog(&state.inner, &mut agent).await;
    let mut workspace = state.inner.workspace.lock().await;
    workspace.root = root;
    workspace.agent = Some(agent);
    save_agent_session(
        &state.inner,
        workspace.agent.as_ref().expect("agent was just assigned"),
    )?;
    drop(workspace);
    Ok(Json(snapshot(&state).await?))
}

async fn delete_session(
    State(state): State<ServerState>,
    Json(request): Json<SessionRequest>,
) -> ApiResult<StateResponse> {
    let mut workspace = state.inner.workspace.lock().await;
    let root = resolve_session_root(&workspace.root, &state.inner.registry, &request)?;
    let store = SessionStore::new(&root)?;
    let sessions = store.list()?;
    let deleting_active = workspace.agent.as_ref().is_some_and(|agent| {
        paths_equal(agent.project_root(), &root)
            && agent.session().id.to_string().starts_with(&request.id)
    });
    let deleted_index = sessions
        .iter()
        .position(|session| session.id.to_string().starts_with(&request.id));
    store.delete(&request.id)?;
    let remaining = store.list()?;

    if deleting_active {
        workspace.root.clone_from(&root);
        workspace.agent = deleted_index
            .and_then(|index| {
                (!remaining.is_empty()).then(|| index.min(remaining.len().saturating_sub(1)))
            })
            .map(|index| store.load(Some(&remaining[index].id.to_string())))
            .transpose()?
            .map(|session| {
                build_agent(
                    &root,
                    &state.inner.config.read().unwrap(),
                    state.inner.debug_openai,
                    session,
                )
            })
            .transpose()?;
    }
    if remaining.is_empty() {
        state.inner.registry.unregister(&root)?;
    }
    drop(workspace);
    Ok(Json(snapshot(&state).await?))
}

fn required_goal_id(request: &GoalRequest) -> std::result::Result<Uuid, ApiError> {
    request
        .id
        .ok_or_else(|| ApiError::message(StatusCode::BAD_REQUEST, "goal id is required"))
}

fn required_goal_objective(request: &GoalRequest) -> std::result::Result<String, ApiError> {
    let objective = request.objective.as_deref().unwrap_or_default().trim();
    if objective.is_empty() {
        return Err(ApiError::message(
            StatusCode::BAD_REQUEST,
            "goal objective is required",
        ));
    }
    if objective.chars().count() > 4_000 {
        return Err(ApiError::message(
            StatusCode::BAD_REQUEST,
            "goal objective cannot exceed 4,000 characters",
        ));
    }
    Ok(objective.to_owned())
}

async fn create_goal(
    State(state): State<ServerState>,
    Json(request): Json<GoalRequest>,
) -> ApiResult<StateResponse> {
    let objective = required_goal_objective(&request)?;
    let mut workspace = state.inner.workspace.lock().await;
    let agent = workspace.agent.as_mut().ok_or_else(|| {
        ApiError::message(
            StatusCode::CONFLICT,
            "create or resume a session before creating a goal",
        )
    })?;
    agent.create_goal(objective);
    save_agent_session(&state.inner, agent)?;
    drop(workspace);
    Ok(Json(snapshot(&state).await?))
}

async fn edit_goal(
    State(state): State<ServerState>,
    Json(request): Json<GoalRequest>,
) -> ApiResult<StateResponse> {
    let id = required_goal_id(&request)?;
    let objective = required_goal_objective(&request)?;
    let mut workspace = state.inner.workspace.lock().await;
    let agent = workspace.agent.as_mut().ok_or_else(|| {
        ApiError::message(
            StatusCode::CONFLICT,
            "create or resume a session before editing a goal",
        )
    })?;
    if !agent.edit_goal(id, objective) {
        return Err(ApiError::message(StatusCode::NOT_FOUND, "goal not found"));
    }
    save_agent_session(&state.inner, agent)?;
    drop(workspace);
    Ok(Json(snapshot(&state).await?))
}

async fn activate_goal(
    State(state): State<ServerState>,
    Json(request): Json<GoalRequest>,
) -> ApiResult<StateResponse> {
    let id = required_goal_id(&request)?;
    let mut workspace = state.inner.workspace.lock().await;
    let agent = workspace.agent.as_mut().ok_or_else(|| {
        ApiError::message(
            StatusCode::CONFLICT,
            "create or resume a session before activating a goal",
        )
    })?;
    if !agent.activate_goal(id) {
        return Err(ApiError::message(StatusCode::NOT_FOUND, "goal not found"));
    }
    save_agent_session(&state.inner, agent)?;
    drop(workspace);
    Ok(Json(snapshot(&state).await?))
}

async fn pause_goal(
    State(state): State<ServerState>,
    Json(request): Json<GoalRequest>,
) -> ApiResult<StateResponse> {
    let id = required_goal_id(&request)?;
    let mut workspace = state.inner.workspace.lock().await;
    let agent = workspace.agent.as_mut().ok_or_else(|| {
        ApiError::message(
            StatusCode::CONFLICT,
            "create or resume a session before pausing a goal",
        )
    })?;
    if !agent.pause_goal(id) {
        return Err(ApiError::message(StatusCode::NOT_FOUND, "goal not found"));
    }
    save_agent_session(&state.inner, agent)?;
    drop(workspace);
    Ok(Json(snapshot(&state).await?))
}

async fn delete_goal(
    State(state): State<ServerState>,
    Json(request): Json<GoalRequest>,
) -> ApiResult<StateResponse> {
    let id = required_goal_id(&request)?;
    let mut workspace = state.inner.workspace.lock().await;
    let agent = workspace.agent.as_mut().ok_or_else(|| {
        ApiError::message(
            StatusCode::CONFLICT,
            "create or resume a session before deleting a goal",
        )
    })?;
    if !agent.delete_goal(id) {
        return Err(ApiError::message(StatusCode::NOT_FOUND, "goal not found"));
    }
    save_agent_session(&state.inner, agent)?;
    drop(workspace);
    Ok(Json(snapshot(&state).await?))
}

async fn index() -> Response {
    asset(INDEX_HTML, "text/html; charset=utf-8", "no-cache")
}

async fn javascript() -> Response {
    asset(APP_JS, "text/javascript; charset=utf-8", "no-cache")
}

async fn stylesheet() -> Response {
    asset(APP_CSS, "text/css; charset=utf-8", "no-cache")
}

fn asset(bytes: &'static [u8], content_type: &'static str, cache: &'static str) -> Response {
    let mut response = bytes.into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}

async fn api_not_found() -> ApiError {
    ApiError::message(StatusCode::NOT_FOUND, "API endpoint not found")
}

type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

struct ApiError {
    status: StatusCode,
    error: anyhow::Error,
}

impl ApiError {
    fn message(status: StatusCode, message: &'static str) -> Self {
        Self {
            status,
            error: anyhow::anyhow!(message),
        }
    }
}

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: error.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error": format!("{:#}", self.error)})),
        )
            .into_response()
    }
}

async fn serve_until_shutdown<F, S>(
    listener: TcpListener,
    app: Router,
    shutdown: F,
    force_shutdown: S,
) -> Result<ShutdownOutcome>
where
    F: Future<Output = Result<()>>,
    S: Future<Output = Result<()>>,
{
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .into_future();
    tokio::pin!(server);
    tokio::pin!(force_shutdown);

    tokio::select! {
        result = server.as_mut() => {
            result.context("web server failed")?;
            return Ok(ShutdownOutcome::Graceful);
        },
        signal = shutdown => signal?,
    }

    let _ = shutdown_tx.send(());
    let warning_delay = tokio::time::sleep(SHUTDOWN_WARNING_DELAY);
    tokio::pin!(warning_delay);
    tokio::select! {
        result = server.as_mut() => {
            result.context("web server failed")?;
            return Ok(ShutdownOutcome::Graceful);
        },
        signal = force_shutdown.as_mut() => {
            signal?;
            return Ok(ShutdownOutcome::Forced);
        },
        () = warning_delay.as_mut() => {}
    }

    eprintln!("{SHUTDOWN_WAITING_MESSAGE}");
    tokio::select! {
        result = server.as_mut() => {
            result.context("web server failed")?;
            Ok(ShutdownOutcome::Graceful)
        },
        signal = force_shutdown.as_mut() => {
            signal?;
            Ok(ShutdownOutcome::Forced)
        },
    }
}

async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c()
        .await
        .context("cannot listen for Ctrl+C")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ActivityKind, ActivityStatus};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::Notify,
    };

    #[test]
    fn embeds_exactly_the_three_web_assets() {
        assert!(INDEX_HTML.starts_with(b"<!doctype html>"));
        assert!(APP_JS.len() > 1_000);
        assert!(APP_CSS.len() > 1_000);
    }

    #[tokio::test]
    async fn cancel_endpoint_signals_the_active_turn() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let store = SessionStore::new(&root).unwrap();
        let session = store
            .create(
                config
                    .provider(&config.active_provider)
                    .unwrap()
                    .model
                    .clone(),
            )
            .unwrap();
        let agent = build_agent(&root, &config, false, session).unwrap();
        let (cancel_tx, mut cancellation) = watch::channel(false);
        let state = ServerState {
            inner: Arc::new(ServerInner {
                config: RwLock::new(config),
                registry: SessionRegistry::at(root.join("test-global-config.toml")),
                debug_openai: false,
                models: RwLock::new(Vec::new()),
                catalog_error: RwLock::new(None),
                dictation_available: false,
                workspace: Mutex::new(ServerWorkspace {
                    root,
                    agent: Some(agent),
                }),
                active_turn: Mutex::new(Some(cancel_tx)),
            }),
        };

        let response = cancel_chat(State(state)).await.0;

        assert_eq!(response["cancelled"], true);
        assert!(*cancellation.borrow_and_update());
    }

    #[tokio::test]
    async fn goal_api_keeps_history_but_only_one_goal_active() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let store = SessionStore::new(&root).unwrap();
        let session = store
            .create(
                config
                    .provider(&config.active_provider)
                    .unwrap()
                    .model
                    .clone(),
            )
            .unwrap();
        let agent = build_agent(&root, &config, false, session).unwrap();
        let state = ServerState {
            inner: Arc::new(ServerInner {
                config: RwLock::new(config),
                registry: SessionRegistry::at(root.join("test-global-config.toml")),
                debug_openai: false,
                models: RwLock::new(Vec::new()),
                catalog_error: RwLock::new(None),
                dictation_available: false,
                workspace: Mutex::new(ServerWorkspace {
                    root,
                    agent: Some(agent),
                }),
                active_turn: Mutex::new(None),
            }),
        };

        let _ = create_goal(
            State(state.clone()),
            Json(GoalRequest {
                id: None,
                objective: Some("First objective".into()),
            }),
        )
        .await
        .ok()
        .unwrap();
        let response = create_goal(
            State(state.clone()),
            Json(GoalRequest {
                id: None,
                objective: Some("Second objective".into()),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;

        assert_eq!(response.session.as_ref().unwrap().goals.len(), 2);
        let active = response
            .session
            .as_ref()
            .unwrap()
            .goals
            .iter()
            .filter(|goal| goal.status == crate::session::GoalStatus::Active)
            .collect::<Vec<_>>();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].objective, "Second objective");
        assert_eq!(
            response.session.as_ref().unwrap().goals[0].status,
            crate::session::GoalStatus::Paused
        );
    }

    #[tokio::test]
    async fn second_shutdown_signal_forces_a_server_with_an_active_request() {
        let request_started = Arc::new(Notify::new());
        let handler_started = request_started.clone();
        let app = Router::new().route(
            "/hold",
            get(move || {
                let handler_started = handler_started.clone();
                async move {
                    handler_started.notify_one();
                    std::future::pending::<String>().await
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (force_tx, force_rx) = oneshot::channel();
        let mut server = tokio::spawn(serve_until_shutdown(
            listener,
            app,
            async move {
                shutdown_rx.await.unwrap();
                Ok(())
            },
            async move {
                force_rx.await.unwrap();
                Ok(())
            },
        ));

        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET /hold HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        request_started.notified().await;
        shutdown_tx.send(()).unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut server)
                .await
                .is_err()
        );
        force_tx.send(()).unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server did not stop after the force signal")
            .unwrap()
            .unwrap();
        assert_eq!(outcome, ShutdownOutcome::Forced);
        assert!(SHUTDOWN_WAITING_MESSAGE.contains("active HTTP requests"));
        assert!(SHUTDOWN_WAITING_MESSAGE.contains("Ctrl+C again"));
    }

    #[tokio::test]
    async fn chat_stream_serializes_deltas_and_activity_as_ndjson() {
        let (sender, mut receiver) = mpsc::channel(1);
        let created_at = chrono::Utc::now();
        assert!(
            send_stream_message(
                &sender,
                ChatStreamMessage::AssistantTextDelta {
                    delta: "hello".into(),
                    start: true,
                    sequence: 12,
                    created_at,
                },
            )
            .await
        );
        let bytes = receiver.recv().await.unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "assistant_text_delta");
        assert_eq!(value["delta"], "hello");
        assert_eq!(value["start"], true);
        assert_eq!(value["sequence"], 12);
        assert_eq!(
            value["created_at"],
            serde_json::to_value(created_at).unwrap()
        );

        assert!(send_stream_message(&sender, ChatStreamMessage::AssistantStreamReset).await);
        let bytes = receiver.recv().await.unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "assistant_stream_reset");

        let activity = AgentActivity {
            id: "call-1".into(),
            turn_message_index: 0,
            sequence: Some(13),
            started_at: Some(created_at),
            completed_at: None,
            tool: "read_file".into(),
            kind: ActivityKind::Read,
            status: ActivityStatus::Running,
            title: "Reading".into(),
            detail: "src/main.rs".into(),
        };
        assert!(send_stream_message(&sender, ChatStreamMessage::Activity { activity }).await);
        let bytes = receiver.recv().await.unwrap().unwrap();
        assert!(bytes.ends_with(b"\n"));
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "activity");
        assert_eq!(value["activity"]["detail"], "src/main.rs");
        assert_eq!(
            value["activity"]["started_at"],
            serde_json::to_value(created_at).unwrap()
        );
    }

    #[tokio::test]
    async fn chat_stream_delivers_live_tool_lifecycle_and_final_state() {
        let responses = [
            json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "I’ll read the note first.",
                        "tool_calls": [{
                            "id": "call_read",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\":\"note.txt\"}"
                            }
                        }]
                    }
                }]
            })
            .to_string(),
            json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "**Finished** reading."
                    }
                }]
            })
            .to_string(),
        ];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let provider_server = tokio::spawn(async move {
            for body in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 16_384];
                let _ = socket.read(&mut request).await.unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("note.txt"), "hello").unwrap();
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
        let store = SessionStore::new(&root).unwrap();
        let session = store
            .create(
                config
                    .provider(&config.active_provider)
                    .unwrap()
                    .model
                    .clone(),
            )
            .unwrap();
        let agent = build_agent(&root, &config, false, session).unwrap();
        let state = ServerState {
            inner: Arc::new(ServerInner {
                config: RwLock::new(config),
                registry: SessionRegistry::at(root.join("test-global-config.toml")),
                debug_openai: false,
                models: RwLock::new(Vec::new()),
                catalog_error: RwLock::new(None),
                dictation_available: false,
                workspace: Mutex::new(ServerWorkspace {
                    root: root.clone(),
                    agent: Some(agent),
                }),
                active_turn: Mutex::new(None),
            }),
        };

        let response = chat(
            State(state),
            Json(ChatRequest {
                prompt: "Read the note".into(),
                continuation: false,
            }),
        )
        .await
        .ok()
        .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let events = std::str::from_utf8(&body)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events[0]["type"], "user_message");
        assert_eq!(events[0]["message"]["content"], "Read the note");
        assert!(events[0]["message"]["created_at"].is_string());
        assert_eq!(events[1]["type"], "assistant_message");
        assert_eq!(events[1]["message"]["content"], "I’ll read the note first.");
        assert_eq!(events[2]["type"], "activity");
        assert_eq!(events[2]["activity"]["status"], "running");
        assert_eq!(events[3]["activity"]["status"], "completed");
        assert_eq!(events[4]["type"], "assistant_message");
        assert_eq!(events[4]["message"]["content"], "**Finished** reading.");
        assert_eq!(events[5]["type"], "done");
        assert_eq!(
            events[5]["state"]["session"]["messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["content"],
            "**Finished** reading."
        );
        provider_server.await.unwrap();
    }

    #[tokio::test]
    async fn completion_api_uses_shared_commands_skills_and_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let skill = root.join(".agents/skills/review-rust");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: review-rust\ndescription: Review Rust changes.\n---\nReview the code.",
        )
        .unwrap();
        std::fs::write(root.join("hello.txt"), "hello").unwrap();

        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let store = SessionStore::new(&root).unwrap();
        let session = store
            .create(
                config
                    .provider(&config.active_provider)
                    .unwrap()
                    .model
                    .clone(),
            )
            .unwrap();
        let agent = build_agent(&root, &config, false, session).unwrap();
        let state = ServerState {
            inner: Arc::new(ServerInner {
                config: RwLock::new(config),
                registry: SessionRegistry::at(root.join("test-global-config.toml")),
                debug_openai: false,
                models: RwLock::new(Vec::new()),
                catalog_error: RwLock::new(None),
                dictation_available: false,
                workspace: Mutex::new(ServerWorkspace {
                    root: root.clone(),
                    agent: Some(agent),
                }),
                active_turn: Mutex::new(None),
            }),
        };

        let slash = completions(
            State(state.clone()),
            Json(CompletionRequest {
                before_cursor: "/".into(),
                after_cursor: String::new(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0
        .unwrap();
        assert!(slash.items.iter().any(|item| item.kind
            == crate::completion::CompletionKind::Command
            && item.name == "help"));
        assert!(
            slash
                .items
                .iter()
                .any(|item| item.kind == crate::completion::CompletionKind::Skill
                    && item.name == "review-rust")
        );

        let files = completions(
            State(state),
            Json(CompletionRequest {
                before_cursor: "@".into(),
                after_cursor: String::new(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0
        .unwrap();
        let hello = files
            .items
            .iter()
            .find(|item| item.name == "hello.txt")
            .unwrap();
        assert_eq!(hello.replacement, "@hello.txt ");
    }

    #[tokio::test]
    async fn resuming_a_web_session_switches_project_and_shared_completions() {
        let temp = tempfile::tempdir().unwrap();
        let current_root = temp.path().join("current");
        let other_root = temp.path().join("other");
        std::fs::create_dir_all(&current_root).unwrap();
        std::fs::create_dir_all(&other_root).unwrap();
        std::fs::write(other_root.join("only-in-other.txt"), "hello").unwrap();

        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let current_store = SessionStore::new(&current_root).unwrap();
        let current = current_store
            .create(
                config
                    .provider(&config.active_provider)
                    .unwrap()
                    .model
                    .clone(),
            )
            .unwrap();
        let other_store = SessionStore::new(&other_root).unwrap();
        let mut other = other_store.create("other-model".into()).unwrap();
        other.title = "Other project session".into();
        other_store.save(&other).unwrap();

        let registry = SessionRegistry::at(temp.path().join("test-global-config.toml"));
        registry.register(&other_root).unwrap();
        let agent = build_agent(&current_root, &config, false, current).unwrap();
        let state = ServerState {
            inner: Arc::new(ServerInner {
                config: RwLock::new(config),
                registry,
                debug_openai: false,
                models: RwLock::new(Vec::new()),
                catalog_error: RwLock::new(None),
                dictation_available: false,
                workspace: Mutex::new(ServerWorkspace {
                    root: current_root.clone(),
                    agent: Some(agent),
                }),
                active_turn: Mutex::new(None),
            }),
        };

        let response = resume_session(
            State(state.clone()),
            Json(SessionRequest {
                project: None,
                id: other.id.to_string(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;

        assert_eq!(response.session.as_ref().unwrap().id, other.id);
        assert!(paths_equal(&response.projects[0].root, &other_root));
        assert!(current_store.list().unwrap().is_empty());
        assert!(paths_equal(
            &state.inner.workspace.lock().await.root,
            &other_root
        ));

        let files = completions(
            State(state),
            Json(CompletionRequest {
                before_cursor: "@".into(),
                after_cursor: String::new(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0
        .unwrap();
        assert!(
            files
                .items
                .iter()
                .any(|item| item.name == "only-in-other.txt")
        );
    }

    #[tokio::test]
    async fn deleting_the_active_web_session_selects_the_next_or_leaves_none() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let store = SessionStore::new(&root).unwrap();
        let mut next = store.create("next-model".into()).unwrap();
        next.updated_at = chrono::Utc::now() - chrono::Duration::seconds(1);
        next.title = "Next saved session".into();
        next.messages.push(crate::provider::Message::text(
            crate::provider::Role::User,
            "next context",
        ));
        let next_id = next.id;
        store.save(&next).unwrap();
        let mut active = store.create("selected-model".into()).unwrap();
        active.updated_at = chrono::Utc::now();
        active.reasoning_effort = Some("high".into());
        active.service_tier = Some("priority".into());
        active.messages.push(crate::provider::Message::text(
            crate::provider::Role::User,
            "old context",
        ));
        let active_id = active.id;
        store.save(&active).unwrap();
        let agent = build_agent(&root, &config, false, active).unwrap();
        let state = ServerState {
            inner: Arc::new(ServerInner {
                config: RwLock::new(config),
                registry: SessionRegistry::at(root.join("test-global-config.toml")),
                debug_openai: false,
                models: RwLock::new(Vec::new()),
                catalog_error: RwLock::new(None),
                dictation_available: false,
                workspace: Mutex::new(ServerWorkspace {
                    root: root.clone(),
                    agent: Some(agent),
                }),
                active_turn: Mutex::new(None),
            }),
        };

        let response = delete_session(
            State(state.clone()),
            Json(SessionRequest {
                project: Some(root.clone()),
                id: active_id.to_string(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;

        let selected = response.session.as_ref().unwrap();
        assert_eq!(selected.id, next_id);
        assert_eq!(selected.model, "next-model");
        assert_eq!(
            selected.messages[0].content.as_deref(),
            Some("next context")
        );
        assert!(
            SessionStore::new(&root)
                .unwrap()
                .load(Some(&active_id.to_string()))
                .is_err()
        );

        let response = delete_session(
            State(state.clone()),
            Json(SessionRequest {
                project: Some(root.clone()),
                id: next_id.to_string(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;

        assert!(response.session.is_none());
        assert!(response.projects[0].sessions.is_empty());
        assert!(state.inner.workspace.lock().await.agent.is_none());
    }
}
