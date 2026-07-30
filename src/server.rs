use std::{
    collections::HashMap,
    convert::Infallible,
    future::{Future, IntoFuture},
    ops::Deref,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Query, State},
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
    sync::{Mutex, mpsc, oneshot},
};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::{
    agent::{Agent, turn_was_cancelled},
    auth::OAuthStore,
    completion::{
        CompletionItem, complete as complete_input, file_completion_context,
        recursive_file_completion_available, start_file_completion_search,
    },
    config::{
        Config, ConfigStore, ProviderConfig, ProviderSummary, SessionRegistry,
        validate_provider_name,
    },
    conversation::{ConversationHandle, ConversationManager, ConversationStatus},
    coordination::SessionCoordinator,
    diagnostics::{DebugOutput, DiagnosticLog},
    events::{AgentActivity, AgentEvent},
    project_fs::{DirectoryListing, browse_directories, create_directory, existing_directory},
    provider::{Message, ModelCatalogEntry, ModelSelection},
    session::{
        Session, SessionProject, SessionStore, list_session_projects, resolve_global_session,
    },
    skills::SkillRegistry,
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
    coordinator: SessionCoordinator,
    config: RwLock<Config>,
    registry: SessionRegistry,
    debug_openai: DebugOutput,
    oauth_logged_in: bool,
    workspace_transition: Mutex<()>,
    workspace: Mutex<ServerWorkspace>,
    conversations: ConversationManager,
    catalogs: RwLock<HashMap<Uuid, CatalogState>>,
}

struct ServerWorkspace {
    root: PathBuf,
    selected_session: Option<Uuid>,
    conversation: Option<ConversationHandle>,
}

#[derive(Clone, Default)]
struct CatalogState {
    models: Vec<ModelCatalogEntry>,
    error: Option<String>,
}

#[derive(Serialize)]
struct StateResponse {
    project: String,
    session: Option<WebSession>,
    projects: Vec<SessionProject>,
    skills: Vec<SkillResponse>,
    models: Vec<ModelCatalogEntry>,
    catalog_error: Option<String>,
    dictation_available: bool,
    providers: Vec<ProviderSummary>,
    workers: Vec<ConversationStatus>,
}

#[derive(Clone)]
struct WebSession(Session);

impl Deref for WebSession {
    type Target = Session;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Serialize for WebSession {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut value = serde_json::to_value(&self.0).map_err(serde::ser::Error::custom)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| serde::ser::Error::custom("session did not serialize as an object"))?;
        object.insert(
            "messages".into(),
            serde_json::to_value(&*self.0.messages).map_err(serde::ser::Error::custom)?,
        );
        object.insert(
            "active_message_ids".into(),
            serde_json::to_value(self.0.messages.active_node_ids())
                .map_err(serde::ser::Error::custom)?,
        );
        object.insert(
            "branch_nodes".into(),
            serde_json::to_value(self.0.messages.visible_user_nodes())
                .map_err(serde::ser::Error::custom)?,
        );
        value.serialize(serializer)
    }
}

#[derive(Serialize)]
struct SkillResponse {
    name: String,
    description: String,
    scope: &'static str,
}

#[derive(Deserialize)]
struct ChatRequest {
    session_id: Option<Uuid>,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    continuation: bool,
    edit_node_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct ConversationRequest {
    session_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct BranchRequest {
    session_id: Option<Uuid>,
    node_id: Uuid,
}

#[derive(Deserialize)]
struct ModelRequest {
    session_id: Option<Uuid>,
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
struct NewSessionRequest {
    project: PathBuf,
}

#[derive(Deserialize)]
struct DirectoryQuery {
    path: Option<PathBuf>,
}

#[derive(Deserialize)]
struct CreateDirectoryRequest {
    parent: PathBuf,
    name: String,
}

#[derive(Deserialize)]
struct OpenProjectRequest {
    path: PathBuf,
}

#[derive(Serialize)]
struct CreatedDirectoryResponse {
    path: PathBuf,
}

#[derive(Deserialize)]
struct CompletionRequest {
    #[serde(default)]
    request_id: u64,
    session_id: Option<Uuid>,
    before_cursor: String,
    after_cursor: String,
}

#[derive(Deserialize)]
struct GoalRequest {
    session_id: Option<Uuid>,
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
    request_id: u64,
    items: Vec<CompletionItem>,
    replace_before: String,
    replace_after: String,
    recursive: bool,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CompletionStreamMessage {
    Update {
        request_id: u64,
        items: Vec<CompletionItem>,
    },
    Done {
        request_id: u64,
    },
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
    debug_openai: DebugOutput,
) -> Result<()> {
    let store = SessionStore::new(&root)?;
    let registry = SessionRegistry::global()?;
    let oauth_logged_in = OAuthStore::new()?.is_logged_in();
    let active = config.provider(&config.active_provider)?;
    let session =
        store.create_for_provider(config.active_provider.clone(), active.model.clone())?;
    let coordinator = SessionCoordinator::new(
        config.clone(),
        registry.clone(),
        debug_openai.clone(),
        DiagnosticLog::stderr(),
        default_instructions_path(&root)?,
    );
    let mut agent = coordinator.build_agent(&root, session)?;
    let (models, catalog_error) = match agent.fetch_models().await {
        Ok(models) => {
            agent.resolve_new_session_model(&models);
            (models, None)
        }
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    list_session_projects(&root, &registry)?;

    let initial_handle = coordinator.install(agent)?;
    let initial_id = initial_handle.snapshot().session.id;
    let manager = coordinator.manager();
    let state = ServerState {
        inner: Arc::new(ServerInner {
            coordinator,
            config: RwLock::new(config),
            registry: registry.clone(),
            debug_openai,
            oauth_logged_in,
            workspace_transition: Mutex::new(()),
            workspace: Mutex::new(ServerWorkspace {
                root,
                selected_session: Some(initial_id),
                conversation: Some(initial_handle),
            }),
            conversations: manager,
            catalogs: RwLock::new(HashMap::from([(
                initial_id,
                CatalogState {
                    models,
                    error: catalog_error,
                },
            )])),
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
                .route("/completions/recursive", post(recursive_completions))
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
                .route("/branches/preview", post(preview_branch))
                .route("/branches/select", post(select_branch))
                .route("/sessions", post(new_session))
                .route("/sessions/delete", post(delete_session))
                .route("/directories", get(list_directories))
                .route("/directories", post(make_directory))
                .route("/projects/open", post(open_project))
                .route("/sessions/resume", post(resume_session))
                .route("/goals/create", post(create_goal))
                .route("/goals/edit", put(edit_goal))
                .route("/goals/activate", post(activate_goal))
                .route("/goals/pause", post(pause_goal))
                .route("/goals/delete", post(delete_goal))
                .fallback(api_not_found),
        )
        .fallback(index)
        .with_state(state.clone());

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
    state.inner.conversations.cancel_all();
    while state
        .inner
        .conversations
        .statuses()
        .iter()
        .any(|status| status.lifecycle == crate::conversation::ConversationLifecycle::Running)
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    state.inner.conversations.shutdown_all().await?;
    Ok(())
}

#[cfg(test)]
fn build_agent(
    root: &std::path::Path,
    config: &Config,
    debug_openai: impl Into<DebugOutput>,
    session: Session,
) -> Result<Agent> {
    build_agent_with_instructions_path(
        root,
        config,
        debug_openai,
        session,
        default_instructions_path(root)?,
    )
}

#[cfg(test)]
fn build_agent_with_instructions_path(
    root: &std::path::Path,
    config: &Config,
    debug_openai: impl Into<DebugOutput>,
    session: Session,
    global_instructions_path: PathBuf,
) -> Result<Agent> {
    let mut provider = crate::provider::OpenAiCompatible::new(config, &session.provider)?;
    provider.set_debug_openai(debug_openai);
    let tools = crate::tools::ToolBox::with_shell(root.to_path_buf(), config.shell.clone());
    Agent::new(
        provider,
        tools,
        SkillRegistry::discover(root),
        session,
        global_instructions_path,
        DiagnosticLog::stderr(),
    )
}

#[cfg(not(test))]
fn default_instructions_path(_root: &std::path::Path) -> Result<PathBuf> {
    Config::instructions_path()
}

#[cfg(test)]
fn default_instructions_path(root: &std::path::Path) -> Result<PathBuf> {
    Ok(root.join(".test-global-config").join("AGENTS.md"))
}

async fn load_catalog(
    agent: &mut Agent,
    new_session: bool,
) -> (Vec<ModelCatalogEntry>, Option<String>) {
    match agent.fetch_models().await {
        Ok(models) => {
            if new_session {
                agent.resolve_new_session_model(&models);
            } else {
                agent.resolve_auto_model(&models);
            }
            (models, None)
        }
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    }
}

fn install_catalog(
    inner: &ServerInner,
    session_id: Uuid,
    (models, error): (Vec<ModelCatalogEntry>, Option<String>),
) {
    inner
        .catalogs
        .write()
        .unwrap()
        .insert(session_id, CatalogState { models, error });
}

async fn install_conversation(
    state: &ServerState,
    root: PathBuf,
    agent: Agent,
) -> Result<ConversationHandle> {
    let conversation = state.inner.conversations.install(agent)?;
    let session_id = conversation.snapshot().session.id;
    let mut workspace = state.inner.workspace.lock().await;
    workspace.root = root;
    workspace.selected_session = Some(session_id);
    workspace.conversation = Some(conversation.clone());
    Ok(conversation)
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

async fn selected_conversation(
    state: &ServerState,
    requested: Option<Uuid>,
) -> Option<ConversationHandle> {
    let selected = if requested.is_some() {
        requested
    } else {
        state.inner.workspace.lock().await.selected_session
    };
    selected.and_then(|id| state.inner.conversations.get(id))
}

async fn snapshot_for(state: &ServerState, requested: Option<Uuid>) -> Result<StateResponse> {
    let (workspace_root, selected_id) = {
        let workspace = state.inner.workspace.lock().await;
        (
            workspace.root.clone(),
            requested.or(workspace.selected_session),
        )
    };
    let conversation = selected_id.and_then(|id| state.inner.conversations.get(id));
    let conversation_snapshot = conversation.as_ref().map(ConversationHandle::snapshot);
    let root = conversation_snapshot
        .as_ref()
        .map(|snapshot| snapshot.project_root.clone())
        .unwrap_or(workspace_root);
    let session = conversation_snapshot
        .as_ref()
        .map(|snapshot| WebSession(snapshot.session.clone()));
    let skills = if let Some(snapshot) = &conversation_snapshot {
        snapshot
            .skills
            .iter()
            .map(|skill| SkillResponse {
                name: skill.name.clone(),
                description: skill.description.clone(),
                scope: skill.scope,
            })
            .collect()
    } else {
        SkillRegistry::discover(&root)
            .skills()
            .iter()
            .map(|skill| SkillResponse {
                name: skill.name.clone(),
                description: skill.description.clone(),
                scope: skill.scope.label(),
            })
            .collect()
    };
    let projects = list_session_projects(&root, &state.inner.registry)?;
    let project = projects
        .first()
        .map(|project| project.root.display().to_string())
        .unwrap_or_else(|| root.display().to_string());
    let config = state.inner.config.read().unwrap();
    let dictation_available = session.as_ref().is_some_and(|session| {
        Transcriber::is_available_with_oauth(
            &config,
            &session.provider,
            state.inner.oauth_logged_in,
        )
        .unwrap_or(false)
    });
    let providers = config.summaries();
    Ok(StateResponse {
        project,
        session,
        projects,
        skills,
        models: selected_id
            .and_then(|id| state.inner.catalogs.read().unwrap().get(&id).cloned())
            .map(|catalog| catalog.models)
            .or_else(|| {
                conversation_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.model_catalog.clone())
            })
            .unwrap_or_default(),
        catalog_error: selected_id
            .and_then(|id| state.inner.catalogs.read().unwrap().get(&id).cloned())
            .and_then(|catalog| catalog.error),
        dictation_available,
        providers,
        workers: state.inner.conversations.statuses(),
    })
}

async fn snapshot(state: &ServerState) -> Result<StateResponse> {
    snapshot_for(state, None).await
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION")
    }))
}

async fn get_state(
    State(state): State<ServerState>,
    Query(request): Query<ConversationRequest>,
) -> ApiResult<StateResponse> {
    if let Some(conversation) = selected_conversation(&state, request.session_id).await {
        conversation.persist_if_idle().await?;
    }
    Ok(Json(snapshot_for(&state, request.session_id).await?))
}

async fn completions(
    State(state): State<ServerState>,
    Json(request): Json<CompletionRequest>,
) -> ApiResult<Option<CompletionResponse>> {
    let cursor = request.before_cursor.len();
    let input = format!("{}{}", request.before_cursor, request.after_cursor);
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before requesting completions",
            )
        })?;
    let snapshot = conversation.snapshot();
    let recursive = recursive_file_completion_available(&input, cursor, &snapshot.project_root);
    let menu = complete_input(
        &input,
        cursor,
        &snapshot.project_root,
        snapshot
            .skills
            .iter()
            .map(|skill| (skill.name.as_str(), skill.description.as_str())),
    );
    let (items, token_start, token_end) = match menu {
        Some(menu) => (menu.items, menu.token_start, menu.token_end),
        None if recursive => {
            let context = file_completion_context(&input, cursor, &snapshot.project_root)
                .expect("recursive availability requires a file completion context");
            (Vec::new(), context.start, context.end)
        }
        None => return Ok(Json(None)),
    };
    Ok(Json(Some(CompletionResponse {
        request_id: request.request_id,
        replace_before: input[token_start..cursor].to_owned(),
        replace_after: input[cursor..token_end].to_owned(),
        items,
        recursive,
    })))
}

async fn recursive_completions(
    State(state): State<ServerState>,
    Json(request): Json<CompletionRequest>,
) -> std::result::Result<Response, ApiError> {
    let cursor = request.before_cursor.len();
    let input = format!("{}{}", request.before_cursor, request.after_cursor);
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before requesting completions",
            )
        })?;
    let snapshot = conversation.snapshot();
    let search =
        start_file_completion_search(&input, cursor, &snapshot.project_root, request.request_id);
    let (output_tx, output_rx) = mpsc::channel::<std::result::Result<Bytes, Infallible>>(8);
    tokio::spawn(async move {
        if let Some(mut search) = search {
            loop {
                let update = tokio::select! {
                    _ = output_tx.closed() => return,
                    update = search.recv() => update,
                };
                let Some(update) = update else {
                    break;
                };
                let message = CompletionStreamMessage::Update {
                    request_id: update.request_id,
                    items: update.items,
                };
                if !send_completion_stream_message(&output_tx, message).await {
                    return;
                }
            }
        }
        let _ = send_completion_stream_message(
            &output_tx,
            CompletionStreamMessage::Done {
                request_id: request.request_id,
            },
        )
        .await;
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

async fn send_completion_stream_message(
    output: &mpsc::Sender<std::result::Result<Bytes, Infallible>>,
    message: CompletionStreamMessage,
) -> bool {
    let mut line = match serde_json::to_vec(&message) {
        Ok(line) => line,
        Err(_) => return false,
    };
    line.push(b'\n');
    output.send(Ok(Bytes::from(line))).await.is_ok()
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
    let requested = headers
        .get("x-codecrab-session")
        .and_then(|value| value.to_str().ok())
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|_| ApiError::message(StatusCode::BAD_REQUEST, "invalid session id"))?;
    let provider = selected_conversation(&state, requested)
        .await
        .map(|conversation| conversation.snapshot().session.provider)
        .context("create or resume a session before using voice dictation")?;
    let config = state.inner.config.read().unwrap().clone();
    if !Transcriber::is_available_with_oauth(&config, &provider, state.inner.oauth_logged_in)? {
        return Err(ApiError::message(
            StatusCode::FORBIDDEN,
            "voice dictation requires the official OpenAI provider and valid authentication",
        ));
    }
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.starts_with("audio/"))
        .unwrap_or("audio/webm");
    let text = Transcriber::new(&config, &provider, state.inner.debug_openai.clone())?
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
    if request.continuation && request.edit_node_id.is_some() {
        return Err(ApiError::message(
            StatusCode::BAD_REQUEST,
            "goal continuation cannot edit a message",
        ));
    }
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before sending a message",
            )
        })?;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let session_id = conversation.snapshot().session.id;
    let turn = if let Some(node_id) = request.edit_node_id {
        conversation.start_edit_turn(node_id, prompt, Some(event_tx))
    } else if request.continuation {
        conversation.start_goal_continuation(Some(event_tx))
    } else {
        conversation.start_turn(prompt, Some(event_tx))
    }
    .map_err(|error| ApiError {
        status: StatusCode::CONFLICT,
        error,
    })?;

    let (output_tx, output_rx) = mpsc::channel::<std::result::Result<Bytes, Infallible>>(32);
    tokio::spawn(async move {
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

        let result = match turn.await {
            Ok(Ok(turn)) => turn.result.map(|_| ()),
            Ok(Err(error)) => Err(error),
            Err(error) => Err(anyhow::anyhow!("conversation turn task failed: {error}")),
        };
        let _ = forward_events.await;

        let message = match result {
            Ok(()) => match snapshot_for(&state, Some(session_id)).await {
                Ok(state) => ChatStreamMessage::Done { state },
                Err(error) => stream_error(error),
            },
            Err(error) if turn_was_cancelled(&error) => {
                match snapshot_for(&state, Some(session_id)).await {
                    Ok(state) => ChatStreamMessage::Cancelled { state },
                    Err(error) => stream_error(error),
                }
            }
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

async fn cancel_chat(
    State(state): State<ServerState>,
    Json(request): Json<ConversationRequest>,
) -> Json<serde_json::Value> {
    let session_id = if request.session_id.is_some() {
        request.session_id
    } else {
        state.inner.workspace.lock().await.selected_session
    };
    let cancelled = session_id.is_some_and(|id| state.inner.conversations.cancel(id));
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
    let _transition = state.inner.workspace_transition.lock().await;
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before changing the model",
            )
        })?;
    let session_id = conversation.snapshot().session.id;
    let catalog = state
        .inner
        .catalogs
        .read()
        .unwrap()
        .get(&session_id)
        .cloned()
        .unwrap_or_default();
    if let Some(model) = catalog
        .models
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
    } else if !catalog.models.is_empty() {
        return Err(ApiError::message(
            StatusCode::BAD_REQUEST,
            "model is not in the provider catalog",
        ));
    }

    conversation
        .set_model(ModelSelection {
            model: request.model,
            reasoning_effort: request.reasoning_effort,
            service_tier: request.service_tier,
        })
        .await?;
    Ok(Json(snapshot_for(&state, Some(session_id)).await?))
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
        state.inner.coordinator.update_config(config.clone());
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
        state.inner.coordinator.update_config(config.clone());
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
        state.inner.coordinator.update_config(config.clone());
        *state.inner.config.write().unwrap() = config;
    }
    Ok(Json(snapshot(&state).await?))
}

async fn clear_session(
    State(state): State<ServerState>,
    Json(request): Json<ConversationRequest>,
) -> ApiResult<StateResponse> {
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before clearing it",
            )
        })?;
    conversation.clear().await?;
    Ok(Json(
        snapshot_for(&state, Some(conversation.snapshot().session.id)).await?,
    ))
}

async fn preview_branch(
    State(state): State<ServerState>,
    Json(request): Json<BranchRequest>,
) -> ApiResult<StateResponse> {
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before browsing its branches",
            )
        })?;
    if conversation.is_running() {
        return Err(ApiError::message(
            StatusCode::CONFLICT,
            "wait for the active turn before browsing conversation branches",
        ));
    }
    let conversation_snapshot = conversation.snapshot();
    let preview = conversation_snapshot
        .session
        .preview_branch(request.node_id)?;
    let mut response = snapshot_for(&state, Some(conversation_snapshot.session.id)).await?;
    response.session = Some(WebSession(preview));
    Ok(Json(response))
}

async fn select_branch(
    State(state): State<ServerState>,
    Json(request): Json<BranchRequest>,
) -> ApiResult<StateResponse> {
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before selecting a conversation branch",
            )
        })?;
    conversation.select_branch(request.node_id).await?;
    Ok(Json(
        snapshot_for(&state, Some(conversation.snapshot().session.id)).await?,
    ))
}

async fn new_session(
    State(state): State<ServerState>,
    Json(request): Json<NewSessionRequest>,
) -> ApiResult<StateResponse> {
    let _transition = state.inner.workspace_transition.lock().await;
    let (workspace_root, current) = {
        let workspace = state.inner.workspace.lock().await;
        (workspace.root.clone(), workspace.conversation.clone())
    };
    let root = existing_directory(&workspace_root, &request.project)?;
    if let Some(current) = current {
        current.persist_if_idle().await?;
    }
    let session = configured_new_session(&state, &root)?;
    let mut agent = state.inner.coordinator.build_agent(&root, session)?;
    let catalog = load_catalog(&mut agent, true).await;
    let conversation = install_conversation(&state, root, agent).await?;
    let session_id = conversation.snapshot().session.id;
    install_catalog(&state.inner, session_id, catalog);
    conversation.persist().await?;
    Ok(Json(snapshot(&state).await?))
}

fn configured_new_session(state: &ServerState, root: &std::path::Path) -> Result<Session> {
    state.inner.coordinator.create_session(root)
}

async fn resume_session(
    State(state): State<ServerState>,
    Json(request): Json<SessionRequest>,
) -> ApiResult<StateResponse> {
    let _transition = state.inner.workspace_transition.lock().await;
    let (current_root, current) = {
        let workspace = state.inner.workspace.lock().await;
        (workspace.root.clone(), workspace.conversation.clone())
    };
    if let Some(current) = current {
        current.persist_if_idle().await?;
    }
    let root = resolve_session_root(&current_root, &state.inner.registry, &request)?;
    let store = SessionStore::new(&root)?;
    let session = store.load(Some(&request.id))?;
    if let Some(existing) = state.inner.conversations.get(session.id) {
        let mut workspace = state.inner.workspace.lock().await;
        workspace.root = root;
        workspace.selected_session = Some(session.id);
        workspace.conversation = Some(existing);
        drop(workspace);
        return Ok(Json(snapshot(&state).await?));
    }
    let mut agent = state.inner.coordinator.build_agent(&root, session)?;
    let catalog = load_catalog(&mut agent, false).await;
    let conversation = install_conversation(&state, root, agent).await?;
    install_catalog(&state.inner, conversation.snapshot().session.id, catalog);
    Ok(Json(snapshot(&state).await?))
}

async fn delete_session(
    State(state): State<ServerState>,
    Json(request): Json<SessionRequest>,
) -> ApiResult<StateResponse> {
    let _transition = state.inner.workspace_transition.lock().await;
    let (current_root, selected_session) = {
        let workspace = state.inner.workspace.lock().await;
        (workspace.root.clone(), workspace.selected_session)
    };
    let root = resolve_session_root(&current_root, &state.inner.registry, &request)?;
    let store = SessionStore::new(&root)?;
    let sessions = store.list()?;
    let target = store.load(Some(&request.id))?;
    let target_id = target.id;
    let deleting_active = selected_session == Some(target_id);
    let deleted_index = sessions.iter().position(|session| session.id == target_id);
    if let Some(conversation) =
        state
            .inner
            .conversations
            .take_if_idle(target_id)
            .map_err(|error| ApiError {
                status: StatusCode::CONFLICT,
                error,
            })?
    {
        conversation.shutdown().await?;
    }

    store.delete(&request.id)?;
    state.inner.catalogs.write().unwrap().remove(&target_id);
    let remaining = store.list()?;
    if deleting_active {
        let replacement_session = deleted_index
            .and_then(|index| {
                (!remaining.is_empty()).then(|| index.min(remaining.len().saturating_sub(1)))
            })
            .map(|index| store.load(Some(&remaining[index].id.to_string())))
            .transpose()?;
        if let Some(session) = replacement_session {
            if let Some(existing) = state.inner.conversations.get(session.id) {
                let mut workspace = state.inner.workspace.lock().await;
                workspace.root.clone_from(&root);
                workspace.selected_session = Some(session.id);
                workspace.conversation = Some(existing);
            } else {
                let mut agent = state.inner.coordinator.build_agent(&root, session)?;
                let catalog = load_catalog(&mut agent, false).await;
                let conversation = install_conversation(&state, root.clone(), agent).await?;
                install_catalog(&state.inner, conversation.snapshot().session.id, catalog);
            }
        } else {
            let mut workspace = state.inner.workspace.lock().await;
            workspace.root.clone_from(&root);
            workspace.selected_session = None;
            workspace.conversation = None;
        }
    }
    Ok(Json(snapshot(&state).await?))
}

async fn list_directories(
    State(state): State<ServerState>,
    Query(query): Query<DirectoryQuery>,
) -> ApiResult<DirectoryListing> {
    let root = state.inner.workspace.lock().await.root.clone();
    Ok(Json(browse_directories(&root, query.path.as_deref())?))
}

async fn make_directory(
    State(state): State<ServerState>,
    Json(request): Json<CreateDirectoryRequest>,
) -> ApiResult<CreatedDirectoryResponse> {
    let root = state.inner.workspace.lock().await.root.clone();
    let path = create_directory(&root, &request.parent, &request.name)?;
    Ok(Json(CreatedDirectoryResponse { path }))
}

async fn open_project(
    State(state): State<ServerState>,
    Json(request): Json<OpenProjectRequest>,
) -> ApiResult<StateResponse> {
    let _transition = state.inner.workspace_transition.lock().await;
    let (current_root, current) = {
        let workspace = state.inner.workspace.lock().await;
        (workspace.root.clone(), workspace.conversation.clone())
    };
    if let Some(current) = current {
        current.persist_if_idle().await?;
    }
    let root = existing_directory(&current_root, &request.path)?;
    state.inner.registry.register(&root)?;
    let mut workspace = state.inner.workspace.lock().await;
    workspace.root = root;
    workspace.selected_session = None;
    workspace.conversation = None;
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
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before creating a goal",
            )
        })?;
    conversation.create_goal(objective).await?;
    Ok(Json(
        snapshot_for(&state, Some(conversation.snapshot().session.id)).await?,
    ))
}

async fn edit_goal(
    State(state): State<ServerState>,
    Json(request): Json<GoalRequest>,
) -> ApiResult<StateResponse> {
    let id = required_goal_id(&request)?;
    let objective = required_goal_objective(&request)?;
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before editing a goal",
            )
        })?;
    if conversation
        .edit_goal(id, objective, false)
        .await?
        .is_none()
    {
        return Err(ApiError::message(StatusCode::NOT_FOUND, "goal not found"));
    }
    Ok(Json(
        snapshot_for(&state, Some(conversation.snapshot().session.id)).await?,
    ))
}

async fn activate_goal(
    State(state): State<ServerState>,
    Json(request): Json<GoalRequest>,
) -> ApiResult<StateResponse> {
    let id = required_goal_id(&request)?;
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before activating a goal",
            )
        })?;
    if conversation.activate_goal(id).await?.is_none() {
        return Err(ApiError::message(StatusCode::NOT_FOUND, "goal not found"));
    }
    Ok(Json(
        snapshot_for(&state, Some(conversation.snapshot().session.id)).await?,
    ))
}

async fn pause_goal(
    State(state): State<ServerState>,
    Json(request): Json<GoalRequest>,
) -> ApiResult<StateResponse> {
    let id = required_goal_id(&request)?;
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before pausing a goal",
            )
        })?;
    if conversation.pause_goal(id).await?.is_none() {
        return Err(ApiError::message(StatusCode::NOT_FOUND, "goal not found"));
    }
    Ok(Json(
        snapshot_for(&state, Some(conversation.snapshot().session.id)).await?,
    ))
}

async fn delete_goal(
    State(state): State<ServerState>,
    Json(request): Json<GoalRequest>,
) -> ApiResult<StateResponse> {
    let id = required_goal_id(&request)?;
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before deleting a goal",
            )
        })?;
    if conversation.delete_goal(id).await?.is_none() {
        return Err(ApiError::message(StatusCode::NOT_FOUND, "goal not found"));
    }
    Ok(Json(
        snapshot_for(&state, Some(conversation.snapshot().session.id)).await?,
    ))
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
    use crate::{
        config::paths_equal,
        events::{ActivityKind, ActivityStatus},
    };
    use tokio::{
        io::AsyncWriteExt,
        net::{TcpListener, TcpStream},
        sync::{Barrier, Notify},
    };

    fn test_conversation(agent: Agent) -> ConversationHandle {
        let registry = SessionRegistry::at(agent.project_root().join("test-global-config.toml"));
        ConversationHandle::spawn(agent, registry).unwrap()
    }

    #[test]
    fn web_sessions_expose_the_active_projection_alongside_the_tree() {
        let root = tempfile::tempdir().unwrap();
        let mut session = SessionStore::new(root.path())
            .unwrap()
            .create("test-model".into())
            .unwrap();
        session.messages.push(Message::text(
            crate::provider::Role::User,
            "Inspect the tree",
        ));

        let value = serde_json::to_value(WebSession(session)).unwrap();
        assert_eq!(value["messages"].as_array().unwrap().len(), 1);
        assert_eq!(value["active_message_ids"].as_array().unwrap().len(), 1);
        assert_eq!(value["branch_nodes"].as_array().unwrap().len(), 1);
        assert_eq!(value["conversation"]["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(
            value["messages"][0]["content"].as_str(),
            Some("Inspect the tree")
        );
    }

    fn test_state(
        config: Config,
        root: PathBuf,
        conversation: ConversationHandle,
        oauth_logged_in: bool,
    ) -> ServerState {
        let registry = SessionRegistry::at(root.join("test-global-config.toml"));
        test_state_with_registry(config, root, conversation, oauth_logged_in, registry)
    }

    fn test_state_with_registry(
        config: Config,
        root: PathBuf,
        conversation: ConversationHandle,
        oauth_logged_in: bool,
        registry: SessionRegistry,
    ) -> ServerState {
        let session_id = conversation.snapshot().session.id;
        let coordinator = SessionCoordinator::new(
            config.clone(),
            registry.clone(),
            DebugOutput::default(),
            DiagnosticLog::stderr(),
            root.join(".test-global-config").join("AGENTS.md"),
        );
        ServerState {
            inner: Arc::new(ServerInner {
                coordinator,
                config: RwLock::new(config),
                registry: registry.clone(),
                debug_openai: DebugOutput::default(),
                oauth_logged_in,
                workspace_transition: Mutex::new(()),
                workspace: Mutex::new(ServerWorkspace {
                    root,
                    selected_session: Some(session_id),
                    conversation: Some(conversation.clone()),
                }),
                conversations: ConversationManager::with_handle(registry, conversation),
                catalogs: RwLock::new(HashMap::new()),
            }),
        }
    }

    #[test]
    fn embeds_exactly_the_three_web_assets() {
        assert!(INDEX_HTML.starts_with(b"<!doctype html>"));
        assert!(APP_JS.len() > 1_000);
        assert!(APP_CSS.len() > 1_000);
    }

    #[tokio::test]
    async fn branch_preview_is_reversible_and_selection_persists_the_resolved_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("model", "http://127.0.0.1:1/v1");
        let store = SessionStore::new(&root).unwrap();
        let mut session = store
            .create_for_provider(config.active_provider.clone(), "model".into())
            .unwrap();
        let root_node = session
            .messages
            .push(Message::text(crate::provider::Role::User, "root"));
        session.messages.push(Message::text(
            crate::provider::Role::Assistant,
            "original answer",
        ));
        let original_leaf = session.messages.push(Message::text(
            crate::provider::Role::User,
            "original follow-up",
        ));
        session
            .messages
            .branch_from(
                Some(root_node),
                Message::text(crate::provider::Role::Assistant, "newer answer"),
            )
            .unwrap();
        let newer_leaf = session.messages.push(Message::text(
            crate::provider::Role::User,
            "newer follow-up",
        ));
        let session_id = session.id;
        let agent = build_agent(&root, &config, false, session).unwrap();
        let conversation = test_conversation(agent);
        let state = test_state(config, root.clone(), conversation.clone(), false);

        let Json(preview) = match preview_branch(
            State(state.clone()),
            Json(BranchRequest {
                session_id: Some(session_id),
                node_id: root_node,
            }),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => panic!("{:#}", error.error),
        };

        assert!(
            preview
                .session
                .as_ref()
                .unwrap()
                .messages
                .active_node_ids()
                .contains(&original_leaf)
        );
        assert!(
            conversation
                .snapshot()
                .session
                .messages
                .active_node_ids()
                .contains(&newer_leaf)
        );

        if let Err(error) = select_branch(
            State(state),
            Json(BranchRequest {
                session_id: Some(session_id),
                node_id: root_node,
            }),
        )
        .await
        {
            panic!("{:#}", error.error);
        }

        assert!(
            conversation
                .snapshot()
                .session
                .messages
                .active_node_ids()
                .contains(&original_leaf)
        );
        let persisted = store.load(Some(&session_id.to_string())).unwrap();
        assert!(
            persisted
                .messages
                .active_node_ids()
                .contains(&original_leaf)
        );
    }

    #[tokio::test]
    async fn transcription_rejects_compatible_providers_before_network_access() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("model", "http://127.0.0.1:1/v1");
        let session = SessionStore::new(&root)
            .unwrap()
            .create_for_provider(config.active_provider.clone(), "model".into())
            .unwrap();
        let agent = build_agent(&root, &config, false, session).unwrap();
        let state = test_state(config, root, test_conversation(agent), true);

        let error =
            match transcribe(State(state), HeaderMap::new(), Bytes::from_static(b"audio")).await {
                Ok(_) => panic!("compatible providers must not expose transcription"),
                Err(error) => error,
            };

        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert!(format!("{:#}", error.error).contains("official OpenAI provider"));
    }

    #[tokio::test]
    async fn cancel_endpoint_signals_the_active_turn() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let blocked_request = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("model", format!("http://{address}/v1"));
        let session = SessionStore::new(&root)
            .unwrap()
            .create_for_provider(config.active_provider.clone(), "model".into())
            .unwrap();
        let agent = build_agent(&root, &config, false, session).unwrap();
        let conversation = test_conversation(agent);
        let turn = conversation.start_turn("Wait".into(), None).unwrap();
        let state = test_state(config, root, conversation, false);
        tokio::time::sleep(Duration::from_millis(25)).await;

        let response = cancel_chat(State(state), Json(ConversationRequest { session_id: None }))
            .await
            .0;
        let outcome = turn.await.unwrap().unwrap();

        assert_eq!(response["cancelled"], true);
        assert!(turn_was_cancelled(outcome.result.as_ref().unwrap_err()));
        blocked_request.abort();
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
        let state = test_state(config, root, test_conversation(agent), false);

        let _ = create_goal(
            State(state.clone()),
            Json(GoalRequest {
                session_id: None,
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
                session_id: None,
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
            turn_message_id: Uuid::nil(),
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
                let _request = crate::test_support::read_http_request(&mut socket).await;
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
        let state = test_state(config, root.clone(), test_conversation(agent), false);

        let response = chat(
            State(state),
            Json(ChatRequest {
                session_id: None,
                prompt: "Read the note".into(),
                continuation: false,
                edit_node_id: None,
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
    async fn editing_a_web_message_streams_a_new_branch_without_discarding_the_original() {
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Answer to the edited request."
                }
            }]
        })
        .to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let provider_server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _request = crate::test_support::read_http_request(&mut socket).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
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
        let mut session = store.create("mock-model".into()).unwrap();
        session
            .messages
            .push(Message::text(crate::provider::Role::User, "Root request"));
        session.messages.push(Message::text(
            crate::provider::Role::Assistant,
            "First answer",
        ));
        let edited_node = session.messages.push(Message::text(
            crate::provider::Role::User,
            "Original follow-up",
        ));
        let original_leaf = session.messages.push(Message::text(
            crate::provider::Role::Assistant,
            "Original continuation",
        ));
        let session_id = session.id;
        let agent = build_agent(&root, &config, false, session).unwrap();
        let conversation = test_conversation(agent);
        let state = test_state(config, root, conversation.clone(), false);

        let response = chat(
            State(state),
            Json(ChatRequest {
                session_id: Some(session_id),
                prompt: "Edited follow-up".into(),
                continuation: false,
                edit_node_id: Some(edited_node),
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
        assert_eq!(events[0]["message"]["content"], "Edited follow-up");
        assert_eq!(events.last().unwrap()["type"], "done");
        let messages = events.last().unwrap()["state"]["session"]["messages"]
            .as_array()
            .unwrap();
        assert_eq!(
            messages
                .iter()
                .filter_map(|message| message["content"].as_str())
                .collect::<Vec<_>>(),
            [
                "Root request",
                "First answer",
                "Edited follow-up",
                "Answer to the edited request."
            ]
        );
        let saved = conversation.snapshot().session;
        assert!(
            saved
                .messages
                .active_node_ids()
                .iter()
                .all(|id| *id != edited_node)
        );
        assert_eq!(
            saved
                .messages
                .message(original_leaf)
                .and_then(|message| message.content.as_deref()),
            Some("Original continuation")
        );
        provider_server.await.unwrap();
    }

    #[tokio::test]
    async fn different_sessions_run_provider_turns_concurrently() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let provider_barrier = barrier.clone();
        let provider_server = tokio::spawn(async move {
            let mut handlers = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let barrier = provider_barrier.clone();
                handlers.push(tokio::spawn(async move {
                    let _request = crate::test_support::read_http_request(&mut socket).await;
                    barrier.wait().await;
                    let body = json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": "Finished independently."
                            }
                        }]
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                }));
            }
            for handler in handlers {
                handler.await.unwrap();
            }
        });

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("mock-model", format!("http://{address}/v1"));
        let store = SessionStore::new(&root).unwrap();
        let first_session = store.create("mock-model".into()).unwrap();
        let first_id = first_session.id;
        let first_agent = build_agent(&root, &config, false, first_session).unwrap();
        let first = test_conversation(first_agent);
        let state = test_state(config.clone(), root.clone(), first, false);

        let second_session = store.create("mock-model".into()).unwrap();
        let second_id = second_session.id;
        let second_agent = build_agent(&root, &config, false, second_session).unwrap();
        state.inner.conversations.install(second_agent).unwrap();

        let first_response = chat(
            State(state.clone()),
            Json(ChatRequest {
                session_id: Some(first_id),
                prompt: "First".into(),
                continuation: false,
                edit_node_id: None,
            }),
        )
        .await
        .ok()
        .unwrap();
        let second_response = chat(
            State(state),
            Json(ChatRequest {
                session_id: Some(second_id),
                prompt: "Second".into(),
                continuation: false,
                edit_node_id: None,
            }),
        )
        .await
        .ok()
        .unwrap();

        tokio::time::timeout(Duration::from_secs(2), barrier.wait())
            .await
            .expect("both provider requests must reach the barrier concurrently");
        let (first_body, second_body) = tokio::join!(
            axum::body::to_bytes(first_response.into_body(), 1_000_000),
            axum::body::to_bytes(second_response.into_body(), 1_000_000),
        );
        let first_body = String::from_utf8(first_body.unwrap().to_vec()).unwrap();
        let second_body = String::from_utf8(second_body.unwrap().to_vec()).unwrap();
        assert!(first_body.contains(&first_id.to_string()));
        assert!(second_body.contains(&second_id.to_string()));
        assert!(first_body.contains("\"type\":\"done\""));
        assert!(second_body.contains("\"type\":\"done\""));
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
        std::fs::create_dir_all(root.join("nested/deeper")).unwrap();
        std::fs::write(root.join("nested/deeper/my-config-file.toml"), "").unwrap();

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
        let state = test_state(config, root.clone(), test_conversation(agent), false);

        let slash = completions(
            State(state.clone()),
            Json(CompletionRequest {
                request_id: 1,
                session_id: None,
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
            State(state.clone()),
            Json(CompletionRequest {
                request_id: 2,
                session_id: None,
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

        let immediate = completions(
            State(state.clone()),
            Json(CompletionRequest {
                request_id: 9,
                session_id: None,
                before_cursor: "@config".into(),
                after_cursor: String::new(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0
        .unwrap();
        assert!(immediate.recursive);
        assert_eq!(immediate.request_id, 9);

        let response = recursive_completions(
            State(state),
            Json(CompletionRequest {
                request_id: 9,
                session_id: None,
                before_cursor: "@config".into(),
                after_cursor: String::new(),
            }),
        )
        .await
        .ok()
        .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let messages = String::from_utf8(body.to_vec()).unwrap();
        assert!(messages.contains("\"type\":\"update\""));
        assert!(messages.contains("\"request_id\":9"));
        assert!(messages.contains("nested/deeper/my-config-file.toml"));
        assert!(messages.contains("\"type\":\"done\""));
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
        let current_id = current.id;
        let other_store = SessionStore::new(&other_root).unwrap();
        let mut other = other_store.create("other-model".into()).unwrap();
        other.title = "Other project session".into();
        other_store.save(&other).unwrap();

        let registry = SessionRegistry::at(temp.path().join("test-global-config.toml"));
        registry.register(&other_root).unwrap();
        let agent = build_agent(&current_root, &config, false, current).unwrap();
        let state = test_state_with_registry(
            config,
            current_root.clone(),
            test_conversation(agent),
            false,
            registry,
        );

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
        assert_eq!(current_store.list().unwrap()[0].id, current_id);
        assert!(paths_equal(
            &state.inner.workspace.lock().await.root,
            &other_root
        ));

        let files = completions(
            State(state),
            Json(CompletionRequest {
                request_id: 3,
                session_id: None,
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
    async fn opening_an_empty_project_persists_it_without_creating_a_session() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("current");
        let empty = temp.path().join("empty");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::create_dir_all(&empty).unwrap();
        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let session = SessionStore::new(&current)
            .unwrap()
            .create("model".into())
            .unwrap();
        let agent = build_agent(&current, &config, false, session).unwrap();
        let registry = SessionRegistry::at(temp.path().join("config.toml"));
        let state = test_state_with_registry(
            config,
            current.clone(),
            test_conversation(agent),
            false,
            registry.clone(),
        );

        let response = open_project(
            State(state.clone()),
            Json(OpenProjectRequest {
                path: empty.clone(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;

        assert!(response.session.is_none());
        assert!(paths_equal(
            &state.inner.workspace.lock().await.root,
            &empty
        ));
        assert!(
            response
                .projects
                .iter()
                .any(|project| paths_equal(&project.root, &empty) && project.sessions.is_empty())
        );
        assert!(
            registry
                .directories()
                .unwrap()
                .iter()
                .any(|root| paths_equal(root, &empty))
        );
    }

    #[tokio::test]
    async fn directory_api_creates_without_opening_and_reports_browse_errors() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("current");
        std::fs::create_dir(&current).unwrap();
        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let session = SessionStore::new(&current)
            .unwrap()
            .create("model".into())
            .unwrap();
        let agent = build_agent(&current, &config, false, session).unwrap();
        let state = test_state(config, current.clone(), test_conversation(agent), false);

        let created = make_directory(
            State(state.clone()),
            Json(CreateDirectoryRequest {
                parent: current.clone(),
                name: "created".into(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;
        assert!(created.path.is_dir());
        assert!(paths_equal(
            &state.inner.workspace.lock().await.root,
            &current
        ));

        let listing = list_directories(
            State(state.clone()),
            Query(DirectoryQuery {
                path: Some(current.join("..")),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;
        assert!(
            listing
                .directories
                .iter()
                .any(|entry| entry.name == "current")
        );

        let error = match list_directories(
            State(state),
            Query(DirectoryQuery {
                path: Some(current.join("missing")),
            }),
        )
        .await
        {
            Ok(_) => panic!("missing directory unexpectedly opened"),
            Err(error) => error,
        };
        assert!(format!("{:#}", error.error).contains("cannot open directory"));
    }

    #[tokio::test]
    async fn new_web_session_targets_the_requested_project_exactly() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("current");
        let target = temp.path().join("target");
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir(&target).unwrap();
        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let session = SessionStore::new(&current)
            .unwrap()
            .create("model".into())
            .unwrap();
        let agent = build_agent(&current, &config, false, session).unwrap();
        let state = test_state(config, current.clone(), test_conversation(agent), false);

        let response = new_session(
            State(state.clone()),
            Json(NewSessionRequest {
                project: target.clone(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;

        let created = response.session.unwrap();
        assert!(paths_equal(
            &state.inner.workspace.lock().await.root,
            &target
        ));
        assert_eq!(
            SessionStore::new(&target).unwrap().list().unwrap()[0].id,
            created.id
        );
        assert!(
            response
                .projects
                .iter()
                .any(|project| paths_equal(&project.root, &target))
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
        let state = test_state(config, root.clone(), test_conversation(agent), false);

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
        assert!(state.inner.workspace.lock().await.conversation.is_none());
    }
}
