use std::{
    collections::HashMap,
    convert::Infallible,
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener as StdTcpListener},
    ops::Deref,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path as AxumPath, Query, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
    routing::{any, get, post, put},
};
use axum_server::{Handle, tls_rustls::RustlsConfig};
use rcgen::{CertificateParams, KeyPair};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncWriteExt,
    net::TcpListener,
    sync::{Mutex, broadcast, mpsc},
};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use uuid::Uuid;

use crate::{
    account_usage::{ResetResult, UsageState, UsageTracker},
    agent::{Agent, turn_was_cancelled},
    attachments::{Attachment, AttachmentStore, MAX_ATTACHMENT_BYTES, validate_sha256},
    auth::OAuthStore,
    browser::{self, OpenBrowserMode},
    changes::ChangeStore,
    code_server::{CodeServerManager, EditorStatus, ExtensionAction, ExtensionDiffFile},
    completion::{
        CompletionItem, ComposerSegment, builtin_command_names,
        complete_with_policy as complete_input, composer_decorations, composer_segments,
        file_completion_context_with_policy, filesystem_root, recursive_file_completion_available,
        slash_completion_range, start_file_completion_search,
    },
    config::{Config, ProviderSummary, SessionRegistry, paths_equal},
    conversation::{
        ConversationHandle, ConversationLifecycle, ConversationLiveEvent, ConversationLiveState,
        ConversationManager, ConversationObservation, ConversationStatus,
    },
    coordination::SessionCoordinator,
    cron::{CronDocument, CronJob, CronSnapshot, CronStore},
    diagnostics::{DebugOutput, DiagnosticLog},
    events::{AgentActivity, AgentEvent},
    project_fs::{DirectoryListing, browse_directories, create_directory, existing_directory},
    provider::{AttachmentBinding, Message, ModelCatalogEntry, ModelSelection},
    session::{
        Session, SessionMetadataUpdate, SessionProject, SessionStore, SessionSummary,
        arrange_session_projects, list_session_projects, resolve_global_session,
    },
    skills::SkillRegistry,
    terminal::{TerminalOutputSnapshot, TerminalProcessState},
    transcription::Transcriber,
};

const INDEX_HTML: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/index.html"));
const APP_JS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/app.js"));
const APP_CSS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/app.css"));
const WEB_MANIFEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/manifest.webmanifest"));
const SERVICE_WORKER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/service-worker.js"));
const APP_ICON_32: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/icon-32.png"));
const APP_ICON_192: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/icon-192.png"));
const APP_ICON_512: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/icon-512.png"));
const SHUTDOWN_WARNING_DELAY: Duration = Duration::from_millis(100);
const SHUTDOWN_WAITING_MESSAGE: &str = "CodeCrab is still shutting down because active HTTP/HTTPS \
requests or open connections have not finished. Press Ctrl+C again to force exit.";
const TERMINAL_SHUTDOWN_WAITING_MESSAGE: &str = "CodeCrab still has managed terminals running. \
Press Ctrl+C again to stop their process trees and force exit.";

#[derive(Debug, Eq, PartialEq)]
enum ShutdownOutcome {
    Graceful,
    Forced,
}

struct TlsMaterial {
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
}

#[derive(Debug)]
struct BoundListeners {
    http: StdTcpListener,
    https: StdTcpListener,
    http_address: SocketAddr,
    https_address: SocketAddr,
}

#[derive(Clone)]
pub(crate) struct ServerState {
    pub(crate) inner: Arc<ServerInner>,
}

pub(crate) struct ServerInner {
    runtime_root: PathBuf,
    coordinator: SessionCoordinator,
    config: RwLock<Config>,
    registry: SessionRegistry,
    debug_openai: DebugOutput,
    oauth_logged_in: bool,
    workspace_transition: Mutex<()>,
    workspace: Mutex<ServerWorkspace>,
    conversations: ConversationManager,
    catalogs: RwLock<HashMap<Uuid, CatalogState>>,
    cron: CronStore,
    usage: UsageTracker,
    pub(crate) code_server: CodeServerManager,
}

struct ServerWorkspace {
    root: PathBuf,
    project_selected: bool,
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
    live_revision: u64,
    project: Option<String>,
    filesystem_root: PathBuf,
    session: Option<WebSession>,
    projects: Vec<SessionProject>,
    skills: Vec<SkillResponse>,
    commands: Vec<&'static str>,
    models: Vec<ModelCatalogEntry>,
    catalog_error: Option<String>,
    dictation_available: bool,
    usage: UsageState,
    providers: Vec<ProviderSummary>,
    workers: Vec<ConversationStatus>,
    cron: Option<CronSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cron_error: Option<String>,
}

#[derive(Clone)]
struct WebSession {
    session: Session,
    observation: Option<ConversationObservation>,
}

impl WebSession {
    fn persisted(session: Session) -> Self {
        Self {
            session,
            observation: None,
        }
    }

    fn live(session: Session, observation: ConversationObservation) -> Self {
        Self {
            session,
            observation: Some(observation),
        }
    }
}

impl Deref for WebSession {
    type Target = Session;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

impl Serialize for WebSession {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut value = serde_json::to_value(&self.session).map_err(serde::ser::Error::custom)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| serde::ser::Error::custom("session did not serialize as an object"))?;
        let live_observation = self.observation.as_ref().filter(|observation| {
            matches!(
                observation.lifecycle,
                ConversationLifecycle::Running | ConversationLifecycle::Stopping
            )
        });
        if let Some(observation) = live_observation {
            object.insert("title".into(), json!(observation.title));
            object.insert("updated_at".into(), json!(observation.latest_event_at));
            object.insert(
                "messages".into(),
                serde_json::to_value(&observation.display_messages)
                    .map_err(serde::ser::Error::custom)?,
            );
            object.insert(
                "activities".into(),
                serde_json::to_value(&observation.activities).map_err(serde::ser::Error::custom)?,
            );
        } else {
            object.insert(
                "messages".into(),
                serde_json::to_value(&*self.session.messages).map_err(serde::ser::Error::custom)?,
            );
        }
        object.insert(
            "active_message_ids".into(),
            serde_json::to_value(self.session.messages.active_node_ids())
                .map_err(serde::ser::Error::custom)?,
        );
        object.insert(
            "branch_nodes".into(),
            serde_json::to_value(self.session.messages.visible_user_nodes())
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
    #[serde(default)]
    attachments: Vec<AttachmentBinding>,
}

#[derive(Deserialize)]
struct AttachmentPreflightRequest {
    project: Option<PathBuf>,
    session_id: Uuid,
    sha256: String,
}

#[derive(Deserialize)]
struct AttachmentUploadQuery {
    project: Option<PathBuf>,
    session_id: Uuid,
    sha256: String,
    name: String,
    mime_type: Option<String>,
}

#[derive(Serialize)]
struct AttachmentResponse {
    attachment: Attachment,
    reference: String,
    reused: bool,
}

#[derive(Deserialize)]
struct ConversationRequest {
    session_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct UsageRequest {
    session_id: Option<Uuid>,
    #[serde(default)]
    coalesce: bool,
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
    #[serde(default)]
    stop_processes: bool,
}

#[derive(Deserialize)]
struct SessionMetadataRequest {
    project: Option<PathBuf>,
    id: String,
    title: Option<String>,
    pinned: Option<bool>,
    archived: Option<bool>,
}

#[derive(Deserialize)]
struct ProcessRequest {
    session_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct StopProcessRequest {
    session_id: Option<Uuid>,
    terminal_id: String,
}

#[derive(Serialize)]
struct ProcessSummary {
    terminal_id: String,
    command: String,
    created_at: chrono::DateTime<chrono::Utc>,
    origin_activity_id: Option<String>,
}

#[derive(Deserialize)]
struct NewSessionRequest {
    project: Option<PathBuf>,
    #[serde(default)]
    no_project: bool,
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
    #[serde(default)]
    skill_refresh_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct GoalRequest {
    session_id: Option<Uuid>,
    id: Option<Uuid>,
    objective: Option<String>,
}

#[derive(Deserialize)]
struct CronJobRequest {
    id: String,
    job: CronJob,
}

#[derive(Deserialize)]
struct CronJobIdRequest {
    id: String,
}

#[derive(Deserialize)]
struct CronEnabledRequest {
    id: String,
    enabled: bool,
}

#[derive(Deserialize)]
struct ProviderSelectionRequest {
    session_id: Option<Uuid>,
    provider: String,
}

#[derive(Deserialize)]
struct ResetUsageRequest {
    session_id: Option<Uuid>,
    idempotency_key: String,
    #[serde(default)]
    credit_id: Option<String>,
}

#[derive(Serialize)]
struct CompletionResponse {
    request_id: u64,
    items: Vec<CompletionItem>,
    replace_before: String,
    replace_after: String,
    recursive: bool,
    slash_context: bool,
    segments: Vec<ComposerSegment>,
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

#[derive(Serialize)]
struct SessionLiveView {
    project_root: Option<PathBuf>,
    lifecycle: ConversationLifecycle,
    observation_revision: u64,
    session: WebSession,
}

impl From<ConversationLiveState> for SessionLiveView {
    fn from(state: ConversationLiveState) -> Self {
        let project_root = (state.snapshot.session.scope == crate::session::SessionScope::Project)
            .then_some(state.snapshot.project_root);
        Self {
            project_root,
            lifecycle: state.lifecycle,
            observation_revision: state.observation.revision,
            session: WebSession::live(state.snapshot.session, state.observation),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionStreamMessage {
    Sync {
        revision: u64,
        projects: Vec<SessionProject>,
        workers: Vec<ConversationStatus>,
        sessions: Vec<SessionLiveView>,
    },
    Catalog {
        revision: u64,
        projects: Vec<SessionProject>,
        workers: Vec<ConversationStatus>,
    },
    Session {
        revision: u64,
        session: Box<SessionLiveView>,
    },
    Event {
        revision: u64,
        session_id: Uuid,
        project_root: PathBuf,
        observation_revision: u64,
        event: Box<ChatStreamMessage>,
    },
}

pub(crate) async fn serve(
    root: PathBuf,
    config: Config,
    host: String,
    port: u16,
    https_port: u16,
    open_browser: Option<OpenBrowserMode>,
    debug_openai: DebugOutput,
) -> Result<()> {
    let registry = SessionRegistry::global()?;
    let oauth_logged_in = OAuthStore::new()?.is_logged_in();
    let coordinator = SessionCoordinator::new(
        config.clone(),
        registry.clone(),
        debug_openai.clone(),
        DiagnosticLog::stderr(),
        root.clone(),
        default_instructions_path(&root)?,
    );
    list_session_projects(&root, &registry)?;
    let manager = coordinator.manager();
    let code_server_path = config.code_server_path.clone();
    let usage = UsageTracker::new(debug_openai.clone())?;
    let state = ServerState {
        inner: Arc::new(ServerInner {
            runtime_root: root.clone(),
            coordinator,
            config: RwLock::new(config),
            registry: registry.clone(),
            debug_openai,
            oauth_logged_in,
            workspace_transition: Mutex::new(()),
            workspace: Mutex::new(ServerWorkspace {
                root,
                project_selected: false,
                selected_session: None,
                conversation: None,
            }),
            conversations: manager,
            catalogs: RwLock::new(HashMap::new()),
            cron: CronStore::default()?,
            usage,
            code_server: CodeServerManager::new(code_server_path)?,
        }),
    };
    let app = server_app(state.clone());

    let tls_config = tls_config(generate_tls_material(&host)?).await?;
    let listeners = bind_listeners(&host, port, https_port).await?;
    let http_origin = display_origin("http", listeners.http_address);
    let https_origin = display_origin("https", listeners.https_address);
    state
        .inner
        .code_server
        .set_control_origin(http_origin.clone());
    println!("HTTP API: {http_origin}/api");
    println!("HTTP Web: {http_origin}/");
    println!("HTTPS API: {https_origin}/api");
    println!("HTTPS Web: {https_origin}/");
    if let Some(mode) = open_browser {
        let url = browser::open(mode, &http_origin, &https_origin)?;
        println!("Opened browser: {url}");
    }

    let shutdown_code_server = state.inner.code_server.clone();
    let graceful_shutdown = async move {
        shutdown_signal().await?;
        shutdown_code_server.shutdown().await;
        Ok(())
    };
    let outcome = serve_until_shutdown(
        listeners,
        tls_config,
        app,
        graceful_shutdown,
        shutdown_signal(),
    )
    .await;
    if matches!(outcome, Ok(ShutdownOutcome::Forced)) {
        let _ = state.inner.conversations.close_all_terminals();
        eprintln!("Forcing CodeCrab to exit immediately.");
        std::process::exit(130);
    }
    state.inner.code_server.shutdown().await;
    let outcome = outcome?;
    debug_assert_eq!(outcome, ShutdownOutcome::Graceful);
    if state.inner.conversations.active_terminal_count() > 0 {
        eprintln!("{TERMINAL_SHUTDOWN_WAITING_MESSAGE}");
        loop {
            tokio::select! {
                signal = shutdown_signal() => {
                    signal?;
                    state.inner.conversations.close_all_terminals()?;
                    eprintln!("Forcing CodeCrab to exit after stopping managed terminals.");
                    std::process::exit(130);
                }
                () = tokio::time::sleep(Duration::from_millis(100)) => {
                    if state.inner.conversations.active_terminal_count() == 0 {
                        break;
                    }
                }
            }
        }
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

fn server_app(state: ServerState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(javascript))
        .route("/app.css", get(stylesheet))
        .route("/manifest.webmanifest", get(web_manifest))
        .route("/service-worker.js", get(service_worker))
        .route("/icon-32.png", get(app_icon_32))
        .route("/icon-192.png", get(app_icon_192))
        .route("/icon-512.png", get(app_icon_512))
        .route(
            "/code-server/{instance_id}/",
            any(crate::code_server_proxy::proxy_root),
        )
        .route(
            "/code-server/{instance_id}/{*tail}",
            any(crate::code_server_proxy::proxy),
        )
        .nest(
            "/api",
            Router::new()
                .route("/health", get(health))
                .route("/state", get(get_state))
                .route("/usage", get(get_usage))
                .route("/usage/reset", post(reset_usage))
                .route("/completions", post(completions))
                .route("/completions/recursive", post(recursive_completions))
                .route("/chat", post(chat))
                .route("/chat/cancel", post(cancel_chat))
                .route("/processes", get(list_processes))
                .route("/processes/stop", post(stop_process))
                .route("/processes/{terminal_id}", get(process_output))
                .route("/attachments/preflight", post(attachment_preflight))
                .route("/attachments/upload", post(upload_attachment))
                .route(
                    "/transcribe",
                    post(transcribe).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
                )
                .route("/model", put(set_model))
                .route("/provider", put(set_provider))
                .route("/branches/preview", post(preview_branch))
                .route("/branches/select", post(select_branch))
                .route("/sessions", post(new_session))
                .route("/sessions/stream", get(session_stream))
                .route("/sessions/metadata", put(update_session_metadata))
                .route("/sessions/delete", post(delete_session))
                .route("/directories", get(list_directories))
                .route("/directories", post(make_directory))
                .route("/projects/open", post(open_project))
                .route("/sessions/resume", post(resume_session))
                .route("/code-server/status", get(code_server_status))
                .route("/code-server/start", post(start_code_server))
                .route("/code-server/restart", post(restart_code_server))
                .route("/code-server/open-change", post(open_file_change))
                .route(
                    "/code-server/extension/{instance_id}/handshake",
                    post(code_server_extension_handshake),
                )
                .route(
                    "/code-server/extension/{instance_id}/commands",
                    get(code_server_extension_commands),
                )
                .route("/goals/create", post(create_goal))
                .route("/goals/edit", put(edit_goal))
                .route("/goals/activate", post(activate_goal))
                .route("/goals/pause", post(pause_goal))
                .route("/goals/delete", post(delete_goal))
                .route("/cron", get(get_cron).put(replace_cron))
                .route("/cron/jobs", post(upsert_cron_job))
                .route("/cron/jobs/delete", post(delete_cron_job))
                .route("/cron/jobs/enabled", put(set_cron_job_enabled))
                .route("/cron/jobs/run", post(run_cron_job))
                .route("/cron/install", post(install_cron))
                .route("/cron/uninstall", post(uninstall_cron))
                .fallback(api_not_found),
        )
        .fallback(index)
        .with_state(state)
}

fn tls_certificate_params(host: &str) -> Result<CertificateParams> {
    CertificateParams::new(tls_subject_alt_names(host))
        .context("cannot configure self-signed HTTPS certificate")
}

fn tls_subject_alt_names(host: &str) -> Vec<String> {
    let mut names = vec![
        "localhost".to_owned(),
        Ipv4Addr::LOCALHOST.to_string(),
        Ipv6Addr::LOCALHOST.to_string(),
    ];
    let configured = host
        .trim()
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host.trim());
    let meaningful = match configured.parse::<IpAddr>() {
        Ok(ip) => !ip.is_unspecified(),
        Err(_) => !configured.is_empty() && configured != "*",
    };
    if meaningful && !names.iter().any(|name| name == configured) {
        names.push(configured.to_owned());
    }
    names
}

fn generate_tls_material(host: &str) -> Result<TlsMaterial> {
    let params = tls_certificate_params(host)?;
    let key_pair = KeyPair::generate().context("cannot generate HTTPS private key")?;
    let certificate = params
        .self_signed(&key_pair)
        .context("cannot generate self-signed HTTPS certificate")?;
    Ok(TlsMaterial {
        certificate_der: certificate.der().to_vec(),
        private_key_der: key_pair.serialize_der(),
    })
}

async fn tls_config(material: TlsMaterial) -> Result<RustlsConfig> {
    RustlsConfig::from_der(vec![material.certificate_der], material.private_key_der)
        .await
        .context("cannot configure HTTPS TLS")
}

async fn bind_listeners(host: &str, port: u16, https_port: u16) -> Result<BoundListeners> {
    let http = TcpListener::bind((host, port))
        .await
        .with_context(|| format!("cannot bind HTTP web server to {host}:{port}"))?;
    let http_address = http
        .local_addr()
        .context("cannot read bound HTTP web server address")?;
    let https = TcpListener::bind((host, https_port))
        .await
        .with_context(|| format!("cannot bind HTTPS web server to {host}:{https_port}"))?;
    let https_address = https
        .local_addr()
        .context("cannot read bound HTTPS web server address")?;
    Ok(BoundListeners {
        http: http
            .into_std()
            .context("cannot prepare HTTP web server listener")?,
        https: https
            .into_std()
            .context("cannot prepare HTTPS web server listener")?,
        http_address,
        https_address,
    })
}

fn display_origin(scheme: &str, address: SocketAddr) -> String {
    let ip = match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    format!("{scheme}://{}", SocketAddr::new(ip, address.port()))
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
    let project_selected = agent.session().scope == crate::session::SessionScope::Project;
    let conversation = state.inner.conversations.install(agent)?;
    let session_id = conversation.snapshot().session.id;
    let mut workspace = state.inner.workspace.lock().await;
    workspace.root = root;
    workspace.project_selected = project_selected;
    workspace.selected_session = Some(session_id);
    workspace.conversation = Some(conversation.clone());
    Ok(conversation)
}

fn resolve_session_root(
    current_root: &std::path::Path,
    registry: &SessionRegistry,
    request: &SessionRequest,
) -> Result<Option<PathBuf>> {
    if let Some(project) = &request.project {
        return Ok(Some(project.clone()));
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

fn live_session_projects(
    root: &std::path::Path,
    inner: &ServerInner,
) -> Result<Vec<SessionProject>> {
    let mut projects = list_session_projects(root, &inner.registry)?;
    for live in inner.conversations.live_states() {
        let session = &live.snapshot.session;
        let summary = SessionSummary {
            id: session.id,
            parent_session_id: session.parent_session_id,
            scheduled_run: session.scheduled_run.clone(),
            created_at: session.created_at,
            updated_at: live.observation.latest_event_at,
            title: live.observation.title,
            manual_title: session.manual_title,
            pinned_at: session.pinned_at,
            archived_at: session.archived_at,
            archived_by_ancestor: false,
            shortcut: false,
            ancestor_titles: Vec::new(),
            model: session.model.clone(),
            depth: 0,
            descendant_count: 0,
            active_terminal_count: session
                .terminals
                .iter()
                .filter(|terminal| terminal.state == TerminalProcessState::Running)
                .count(),
        };
        let live_root = (session.scope == crate::session::SessionScope::Project)
            .then_some(&live.snapshot.project_root);
        if let Some(project) =
            projects
                .iter_mut()
                .find(|project| match (project.root.as_ref(), live_root) {
                    (Some(root), Some(live_root)) => paths_equal(root, live_root),
                    (None, None) => true,
                    _ => false,
                })
        {
            if let Some(existing) = project
                .sessions
                .iter_mut()
                .find(|existing| existing.id == session.id)
            {
                *existing = summary;
            } else {
                project.sessions.push(summary);
            }
        } else {
            projects.push(SessionProject {
                root: live_root.cloned(),
                sessions: vec![summary],
            });
        }
    }
    arrange_session_projects(&mut projects);
    Ok(projects)
}

async fn snapshot_for(state: &ServerState, requested: Option<Uuid>) -> Result<StateResponse> {
    let live_revision = state.inner.conversations.live_revision();
    let (workspace_root, project_selected, selected_id) = {
        let workspace = state.inner.workspace.lock().await;
        (
            workspace.root.clone(),
            workspace.project_selected,
            requested.or(workspace.selected_session),
        )
    };
    let conversation = selected_id.and_then(|id| state.inner.conversations.get(id));
    let conversation_state = conversation.as_ref().map(ConversationHandle::live_state);
    let conversation_snapshot = conversation_state
        .as_ref()
        .map(|state| state.snapshot.clone());
    let root = conversation_snapshot
        .as_ref()
        .map(|snapshot| snapshot.project_root.clone())
        .unwrap_or(workspace_root);
    let session = conversation_state
        .as_ref()
        .map(|state| WebSession::live(state.snapshot.session.clone(), state.observation.clone()));
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
        let skills = if project_selected {
            SkillRegistry::discover(&root)
        } else {
            SkillRegistry::discover_global()
        };
        skills
            .skills()
            .iter()
            .map(|skill| SkillResponse {
                name: skill.name.clone(),
                description: skill.description.clone(),
                scope: skill.scope.label(),
            })
            .collect()
    };
    let projects = live_session_projects(&root, &state.inner)?;
    let project = conversation_snapshot
        .as_ref()
        .and_then(|snapshot| {
            (snapshot.session.scope == crate::session::SessionScope::Project)
                .then(|| snapshot.project_root.display().to_string())
        })
        .or_else(|| project_selected.then(|| root.display().to_string()));
    let config = state.inner.config.read().unwrap().clone();
    let dictation_available = session.as_ref().is_some_and(|session| {
        Transcriber::is_available_with_oauth(
            &config,
            &session.provider,
            state.inner.oauth_logged_in,
        )
        .unwrap_or(false)
    });
    let providers = config.summaries();
    let usage = if let Some(session) = &session {
        state
            .inner
            .usage
            .current_for(&config, &session.provider)
            .await
    } else {
        UsageState::hidden()
    };
    let (cron, cron_error) = match state.inner.cron.snapshot(chrono::Utc::now()) {
        Ok(snapshot) => (Some(snapshot), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    Ok(StateResponse {
        live_revision,
        project,
        filesystem_root: filesystem_root(&root),
        session,
        projects,
        skills,
        commands: builtin_command_names().collect(),
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
        usage,
        providers,
        workers: state.inner.conversations.statuses(),
        cron,
        cron_error,
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

async fn get_usage(
    State(state): State<ServerState>,
    Query(request): Query<UsageRequest>,
) -> ApiResult<UsageState> {
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before requesting usage",
            )
        })?;
    let provider = conversation.snapshot().session.provider;
    let config = state.inner.config.read().unwrap().clone();
    let usage = if request.coalesce {
        state
            .inner
            .usage
            .refresh_coalesced_for(&config, &provider)
            .await
    } else {
        state.inner.usage.refresh_for(&config, &provider).await
    };
    Ok(Json(usage))
}

async fn reset_usage(
    State(state): State<ServerState>,
    Json(request): Json<ResetUsageRequest>,
) -> ApiResult<ResetResult> {
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before resetting usage",
            )
        })?;
    let provider = conversation.snapshot().session.provider;
    let config = state.inner.config.read().unwrap().clone();
    let result = state
        .inner
        .usage
        .reset_for(
            &config,
            &provider,
            &request.idempotency_key,
            request.credit_id.as_deref(),
        )
        .await
        .map_err(|error| ApiError {
            status: StatusCode::CONFLICT,
            error,
        })?;
    Ok(Json(result))
}

async fn get_cron(State(state): State<ServerState>) -> ApiResult<CronSnapshot> {
    Ok(Json(state.inner.cron.snapshot(chrono::Utc::now())?))
}

async fn replace_cron(
    State(state): State<ServerState>,
    Json(document): Json<CronDocument>,
) -> ApiResult<CronSnapshot> {
    state.inner.cron.save_document(&document).await?;
    Ok(Json(state.inner.cron.snapshot(chrono::Utc::now())?))
}

async fn upsert_cron_job(
    State(state): State<ServerState>,
    Json(request): Json<CronJobRequest>,
) -> ApiResult<CronSnapshot> {
    Ok(Json(
        state.inner.cron.upsert(&request.id, request.job).await?,
    ))
}

async fn delete_cron_job(
    State(state): State<ServerState>,
    Json(request): Json<CronJobIdRequest>,
) -> ApiResult<CronSnapshot> {
    if !state.inner.cron.delete(&request.id).await? {
        return Err(ApiError::message(
            StatusCode::NOT_FOUND,
            "cron job does not exist",
        ));
    }
    Ok(Json(state.inner.cron.snapshot(chrono::Utc::now())?))
}

async fn set_cron_job_enabled(
    State(state): State<ServerState>,
    Json(request): Json<CronEnabledRequest>,
) -> ApiResult<CronSnapshot> {
    Ok(Json(
        state
            .inner
            .cron
            .set_enabled(&request.id, request.enabled)
            .await?,
    ))
}

async fn run_cron_job(
    State(state): State<ServerState>,
    Json(request): Json<CronJobIdRequest>,
) -> ApiResult<CronSnapshot> {
    Ok(Json(
        state
            .inner
            .cron
            .run_now(&request.id, state.inner.coordinator.clone())
            .await?,
    ))
}

async fn install_cron(State(state): State<ServerState>) -> ApiResult<CronSnapshot> {
    state.inner.cron.install()?;
    Ok(Json(state.inner.cron.snapshot(chrono::Utc::now())?))
}

async fn uninstall_cron(State(state): State<ServerState>) -> ApiResult<CronSnapshot> {
    state.inner.cron.uninstall()?;
    Ok(Json(state.inner.cron.snapshot(chrono::Utc::now())?))
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
    let absolute_only = snapshot.session.scope == crate::session::SessionScope::NoProject;
    let recursive =
        recursive_file_completion_available(&input, cursor, &snapshot.project_root, absolute_only);
    let slash_range = slash_completion_range(&input, cursor);
    let skills = if let (Some(refresh_id), Some(_)) = (request.skill_refresh_id, slash_range) {
        conversation.queue_skill_refresh_once(refresh_id)?
    } else {
        snapshot.skills.clone()
    };
    let usage_available = state.inner.usage.available_for(
        &state.inner.config.read().unwrap(),
        &snapshot.session.provider,
    );
    let menu = complete_input(
        &input,
        cursor,
        &snapshot.project_root,
        skills
            .iter()
            .map(|skill| (skill.name.as_str(), skill.description.as_str())),
        usage_available,
        absolute_only,
    );
    let decorations = composer_decorations(
        &input,
        &snapshot.project_root,
        skills
            .iter()
            .map(|skill| (skill.name.as_str(), skill.description.as_str())),
        usage_available,
        absolute_only,
    );
    let segments = composer_segments(&input, &decorations);
    let (items, token_start, token_end) = match menu {
        Some(menu) => (menu.items, menu.token_start, menu.token_end),
        None if recursive => {
            let context = file_completion_context_with_policy(
                &input,
                cursor,
                &snapshot.project_root,
                absolute_only,
            )
            .expect("recursive availability requires a file completion context");
            (Vec::new(), context.start, context.end)
        }
        None if slash_range.is_some() => {
            let (start, end) = slash_range.expect("checked above");
            (Vec::new(), start, end)
        }
        None => (Vec::new(), cursor, cursor),
    };
    Ok(Json(Some(CompletionResponse {
        request_id: request.request_id,
        replace_before: input[token_start..cursor].to_owned(),
        replace_after: input[cursor..token_end].to_owned(),
        items,
        recursive,
        slash_context: slash_range.is_some(),
        segments,
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
    let search = start_file_completion_search(
        &input,
        cursor,
        &snapshot.project_root,
        request.request_id,
        snapshot.session.scope == crate::session::SessionScope::NoProject,
    );
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

fn attachment_conversation(
    state: &ServerState,
    project: Option<&std::path::Path>,
    session_id: Uuid,
) -> std::result::Result<(ConversationHandle, AttachmentStore), ApiError> {
    let conversation = state.inner.conversations.get(session_id).ok_or_else(|| {
        ApiError::message(
            StatusCode::CONFLICT,
            "resume the target session before attaching files",
        )
    })?;
    let snapshot = conversation.snapshot();
    let matches_scope = match (snapshot.session.scope, project) {
        (crate::session::SessionScope::NoProject, None) => true,
        (crate::session::SessionScope::Project, Some(project)) => {
            paths_equal(&snapshot.project_root, project)
        }
        _ => false,
    };
    if snapshot.session.id != session_id || !matches_scope {
        return Err(ApiError::message(
            StatusCode::CONFLICT,
            "attachment project and session do not match",
        ));
    }
    let store = SessionStore::for_project_root_in(
        (snapshot.session.scope == crate::session::SessionScope::Project)
            .then_some(snapshot.project_root.as_path()),
        &state.inner.registry.data_dir()?,
    )?
    .attachment_store();
    Ok((conversation, store))
}

async fn attachment_preflight(
    State(state): State<ServerState>,
    Json(request): Json<AttachmentPreflightRequest>,
) -> ApiResult<AttachmentResponse> {
    validate_sha256(&request.sha256).map_err(|error| ApiError {
        status: StatusCode::BAD_REQUEST,
        error,
    })?;
    let (conversation, store) =
        attachment_conversation(&state, request.project.as_deref(), request.session_id)?;
    let snapshot = conversation.snapshot();
    let attachment = AttachmentStore::find_by_hash(&snapshot.session.attachments, &request.sha256)
        .cloned()
        .ok_or_else(|| ApiError::message(StatusCode::NOT_FOUND, "attachment hash is not stored"))?;
    let reference = store.visible_reference(&attachment);
    Ok(Json(AttachmentResponse {
        attachment,
        reference,
        reused: true,
    }))
}

async fn upload_attachment(
    State(state): State<ServerState>,
    Query(request): Query<AttachmentUploadQuery>,
    body: Body,
) -> ApiResult<AttachmentResponse> {
    validate_sha256(&request.sha256).map_err(|error| ApiError {
        status: StatusCode::BAD_REQUEST,
        error,
    })?;
    let (conversation, store) =
        attachment_conversation(&state, request.project.as_deref(), request.session_id)?;
    let snapshot = conversation.snapshot();
    if let Some(attachment) =
        AttachmentStore::find_by_hash(&snapshot.session.attachments, &request.sha256).cloned()
    {
        let reference = store.visible_reference(&attachment);
        return Ok(Json(AttachmentResponse {
            attachment,
            reference,
            reused: true,
        }));
    }

    let temp_path = store.upload_temp_path(request.session_id)?;
    let upload_result = async {
        let mut file = tokio::fs::File::create(&temp_path).await?;
        let mut stream = body.into_data_stream();
        let mut size = 0_u64;
        let mut digest = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("cannot read attachment upload")?;
            size = size.saturating_add(chunk.len() as u64);
            if size > MAX_ATTACHMENT_BYTES {
                anyhow::bail!(
                    "attachment exceeds the {} MiB limit",
                    MAX_ATTACHMENT_BYTES / 1024 / 1024
                );
            }
            digest.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);
        if size == 0 {
            anyhow::bail!("attachment is empty");
        }
        let actual = format!("{:x}", digest.finalize());
        if !actual.eq_ignore_ascii_case(&request.sha256) {
            anyhow::bail!("attachment SHA-256 does not match the uploaded bytes");
        }
        let attachment = store.import_uploaded_file(
            request.session_id,
            &snapshot.session.attachments,
            &temp_path,
            &request.name,
            request.mime_type.as_deref(),
            &request.sha256,
        )?;
        let (_, attachment) = conversation.add_attachment(attachment).await?;
        let reference = store.visible_reference(&attachment);
        Ok::<_, anyhow::Error>(AttachmentResponse {
            attachment,
            reference,
            reused: false,
        })
    }
    .await;
    let _ = tokio::fs::remove_file(&temp_path).await;
    match upload_result {
        Ok(response) => Ok(Json(response)),
        Err(error) => Err(ApiError {
            status: if format!("{error:#}").contains("limit") {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            },
            error,
        }),
    }
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
    let provider = conversation.snapshot().session.provider;
    let turn = if let Some(node_id) = request.edit_node_id {
        conversation.start_edit_turn_with_attachments(
            node_id,
            prompt,
            request.attachments,
            Some(event_tx),
        )
    } else if request.continuation {
        conversation.start_goal_continuation(Some(event_tx))
    } else {
        conversation.start_turn_with_attachments(prompt, request.attachments, Some(event_tx))
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
                let message = chat_stream_event(event);
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
            Ok(()) => {
                let config = state.inner.config.read().unwrap().clone();
                state
                    .inner
                    .usage
                    .refresh_in_background(config, provider.clone());
                match snapshot_for(&state, Some(session_id)).await {
                    Ok(state) => ChatStreamMessage::Done { state },
                    Err(error) => stream_error(error),
                }
            }
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

async fn list_processes(
    State(state): State<ServerState>,
    Query(request): Query<ProcessRequest>,
) -> ApiResult<Vec<ProcessSummary>> {
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .context("create or resume a session before listing processes")?;
    Ok(Json(
        conversation
            .running_terminals()
            .into_iter()
            .map(|terminal| ProcessSummary {
                terminal_id: terminal.id,
                command: terminal.command,
                created_at: terminal.created_at,
                origin_activity_id: terminal.origin_activity_id,
            })
            .collect(),
    ))
}

async fn process_output(
    State(state): State<ServerState>,
    AxumPath(terminal_id): AxumPath<String>,
    Query(request): Query<ProcessRequest>,
) -> ApiResult<TerminalOutputSnapshot> {
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .context("create or resume a session before viewing process output")?;
    Ok(Json(conversation.terminal_output(&terminal_id)?))
}

async fn stop_process(
    State(state): State<ServerState>,
    Json(request): Json<StopProcessRequest>,
) -> ApiResult<TerminalOutputSnapshot> {
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .context("create or resume a session before stopping a process")?;
    conversation.close_terminal(&request.terminal_id)?;
    Ok(Json(conversation.terminal_output(&request.terminal_id)?))
}

async fn session_stream(State(state): State<ServerState>) -> Response {
    let mut live_events = state.inner.conversations.subscribe_live();
    let (output_tx, output_rx) = mpsc::channel::<std::result::Result<Bytes, Infallible>>(32);
    tokio::spawn(async move {
        let revision = state.inner.conversations.live_revision();
        let Ok(initial) = session_sync_message(&state, revision).await else {
            return;
        };
        if !send_ndjson(&output_tx, &initial).await {
            return;
        }
        loop {
            let envelope = tokio::select! {
                _ = output_tx.closed() => break,
                event = live_events.recv() => event,
            };
            let envelope = match envelope {
                Ok(envelope) => envelope,
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let revision = state.inner.conversations.live_revision();
                    let Ok(message) = session_sync_message(&state, revision).await else {
                        continue;
                    };
                    if !send_ndjson(&output_tx, &message).await {
                        break;
                    }
                    continue;
                }
            };
            match envelope.event {
                ConversationLiveEvent::Agent(event) => {
                    let message = SessionStreamMessage::Event {
                        revision: envelope.revision,
                        session_id: envelope.session_id,
                        project_root: envelope.project_root,
                        observation_revision: envelope.observation_revision,
                        event: Box::new(chat_stream_event(event)),
                    };
                    if !send_ndjson(&output_tx, &message).await {
                        break;
                    }
                }
                ConversationLiveEvent::Lifecycle => {
                    let Some(handle) = state.inner.conversations.get(envelope.session_id) else {
                        continue;
                    };
                    let message = SessionStreamMessage::Session {
                        revision: envelope.revision,
                        session: Box::new(handle.live_state().into()),
                    };
                    if !send_ndjson(&output_tx, &message).await {
                        break;
                    }
                }
                ConversationLiveEvent::Installed
                | ConversationLiveEvent::Snapshot
                | ConversationLiveEvent::Terminals => {
                    let Ok(catalog) = session_catalog_message(&state, envelope.revision).await
                    else {
                        continue;
                    };
                    if !send_ndjson(&output_tx, &catalog).await {
                        break;
                    }
                    let Some(handle) = state.inner.conversations.get(envelope.session_id) else {
                        continue;
                    };
                    let message = SessionStreamMessage::Session {
                        revision: envelope.revision,
                        session: Box::new(handle.live_state().into()),
                    };
                    if !send_ndjson(&output_tx, &message).await {
                        break;
                    }
                }
                ConversationLiveEvent::Removed => {
                    let Ok(message) = session_catalog_message(&state, envelope.revision).await
                    else {
                        continue;
                    };
                    if !send_ndjson(&output_tx, &message).await {
                        break;
                    }
                }
            }
        }
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
    response
}

async fn session_sync_message(state: &ServerState, revision: u64) -> Result<SessionStreamMessage> {
    let (root, selected_session) = {
        let workspace = state.inner.workspace.lock().await;
        (workspace.root.clone(), workspace.selected_session)
    };
    Ok(SessionStreamMessage::Sync {
        revision,
        projects: live_session_projects(&root, &state.inner)?,
        workers: state.inner.conversations.statuses(),
        sessions: state
            .inner
            .conversations
            .live_states()
            .into_iter()
            .filter(|state| {
                Some(state.snapshot.session.id) == selected_session
                    || matches!(
                        state.lifecycle,
                        ConversationLifecycle::Running | ConversationLifecycle::Stopping
                    )
            })
            .map(Into::into)
            .collect(),
    })
}

async fn session_catalog_message(
    state: &ServerState,
    revision: u64,
) -> Result<SessionStreamMessage> {
    let root = state.inner.workspace.lock().await.root.clone();
    Ok(SessionStreamMessage::Catalog {
        revision,
        projects: live_session_projects(&root, &state.inner)?,
        workers: state.inner.conversations.statuses(),
    })
}

fn chat_stream_event(event: AgentEvent) -> ChatStreamMessage {
    match event {
        AgentEvent::UserMessage(message) => ChatStreamMessage::UserMessage { message },
        AgentEvent::AssistantMessage(message) => ChatStreamMessage::AssistantMessage { message },
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
    }
}

async fn send_ndjson(
    output: &mpsc::Sender<std::result::Result<Bytes, Infallible>>,
    message: &impl Serialize,
) -> bool {
    let mut line = match serde_json::to_vec(message) {
        Ok(line) => line,
        Err(_) => return false,
    };
    line.push(b'\n');
    output.send(Ok(Bytes::from(line))).await.is_ok()
}

async fn send_stream_message(
    output: &mpsc::Sender<std::result::Result<Bytes, Infallible>>,
    message: ChatStreamMessage,
) -> bool {
    send_ndjson(output, &message).await
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

async fn set_provider(
    State(state): State<ServerState>,
    Json(request): Json<ProviderSelectionRequest>,
) -> ApiResult<StateResponse> {
    let _transition = state.inner.workspace_transition.lock().await;
    let conversation = selected_conversation(&state, request.session_id)
        .await
        .ok_or_else(|| {
            ApiError::message(
                StatusCode::CONFLICT,
                "create or resume a session before changing the provider",
            )
        })?;
    let session_id = conversation.snapshot().session.id;
    let (provider, configured_model) = state.inner.coordinator.build_provider(&request.provider)?;
    let updated = conversation
        .set_provider(request.provider, configured_model, provider)
        .await?;
    state.inner.catalogs.write().unwrap().insert(
        session_id,
        CatalogState {
            models: updated.model_catalog,
            error: None,
        },
    );
    Ok(Json(snapshot_for(&state, Some(session_id)).await?))
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
    response.session = Some(WebSession::persisted(preview));
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
    if request.no_project && request.project.is_some() {
        return Err(ApiError::message(
            StatusCode::BAD_REQUEST,
            "project and no_project cannot both be set",
        ));
    }
    let root = if request.no_project {
        state.inner.runtime_root.clone()
    } else {
        let project = request
            .project
            .as_deref()
            .ok_or_else(|| ApiError::message(StatusCode::BAD_REQUEST, "project is required"))?;
        existing_directory(&workspace_root, project)?
    };
    let session = if request.no_project {
        state.inner.coordinator.create_no_project_session()?
    } else {
        configured_new_session(&state, &root)?
    };
    let mut agent = state.inner.coordinator.build_agent(&root, session)?;
    let catalog = load_catalog(&mut agent, true).await;
    if let Some(current) = current {
        state
            .inner
            .conversations
            .prepare_for_navigation(current.snapshot().session.id)
            .await?;
    }
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
    let session_root = resolve_session_root(&current_root, &state.inner.registry, &request)?;
    let store = SessionStore::for_project_root_in(
        session_root.as_deref(),
        &state.inner.registry.data_dir()?,
    )?;
    let session = store.load(Some(&request.id))?;
    if let Some(current) = current
        && current.snapshot().session.id != session.id
    {
        state
            .inner
            .conversations
            .prepare_for_navigation(current.snapshot().session.id)
            .await?;
    }
    let root = session_root.unwrap_or_else(|| state.inner.runtime_root.clone());
    if let Some(existing) = state.inner.conversations.get(session.id) {
        let mut workspace = state.inner.workspace.lock().await;
        workspace.root = root;
        workspace.project_selected = session.scope == crate::session::SessionScope::Project;
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

async fn update_session_metadata(
    State(state): State<ServerState>,
    Json(request): Json<SessionMetadataRequest>,
) -> ApiResult<StateResponse> {
    let updates = usize::from(request.title.is_some())
        + usize::from(request.pinned.is_some())
        + usize::from(request.archived.is_some());
    if updates != 1 {
        return Err(ApiError::message(
            StatusCode::BAD_REQUEST,
            "provide exactly one of title, pinned, or archived",
        ));
    }
    let update = if let Some(title) = request.title.clone() {
        SessionMetadataUpdate::Rename(title)
    } else if let Some(pinned) = request.pinned {
        SessionMetadataUpdate::SetPinned(pinned)
    } else {
        SessionMetadataUpdate::SetArchived(request.archived.expect("one update was provided"))
    };
    let current_root = state.inner.workspace.lock().await.root.clone();
    let session_request = SessionRequest {
        project: request.project,
        id: request.id,
        stop_processes: false,
    };
    let session_root =
        resolve_session_root(&current_root, &state.inner.registry, &session_request)?;
    let store = SessionStore::for_project_root_in(
        session_root.as_deref(),
        &state.inner.registry.data_dir()?,
    )?;
    let target = store.load(Some(&session_request.id))?;
    if let Some(conversation) = state.inner.conversations.get(target.id) {
        conversation.update_metadata(update).await?;
    } else {
        let mut target = target;
        target.update_metadata(update, chrono::Utc::now())?;
        store.save(&target)?;
        state.inner.conversations.publish_catalog_change(
            target.id,
            session_root.unwrap_or_else(|| state.inner.runtime_root.clone()),
        );
    }
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
    let session_root = resolve_session_root(&current_root, &state.inner.registry, &request)?;
    let store = SessionStore::for_project_root_in(
        session_root.as_deref(),
        &state.inner.registry.data_dir()?,
    )?;
    let root = session_root.unwrap_or_else(|| state.inner.runtime_root.clone());
    let sessions = store.list()?;
    let target = store.load(Some(&request.id))?;
    let target_id = target.id;
    let deleting_active = selected_session == Some(target_id);
    let deleted_index = sessions.iter().position(|session| session.id == target_id);
    if let Some(conversation) = state.inner.conversations.get(target_id) {
        let active_terminals = conversation.running_terminal_count();
        if active_terminals > 0 && !request.stop_processes {
            return Err(ApiError::message(
                StatusCode::CONFLICT,
                "confirm stopping active processes before deleting this session",
            ));
        }
        if active_terminals > 0 {
            conversation.close_terminals()?;
        }
    }
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

    store.discard(target_id)?;
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
                workspace.project_selected = session.scope == crate::session::SessionScope::Project;
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
            workspace.project_selected = target.scope == crate::session::SessionScope::Project;
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
    let root = existing_directory(&current_root, &request.path)?;
    if let Some(current) = current {
        state
            .inner
            .conversations
            .prepare_for_navigation(current.snapshot().session.id)
            .await?;
    }
    state.inner.registry.register(&root)?;
    let mut workspace = state.inner.workspace.lock().await;
    workspace.root = root;
    workspace.project_selected = true;
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

async fn web_manifest() -> Response {
    asset(
        WEB_MANIFEST,
        "application/manifest+json; charset=utf-8",
        "no-cache",
    )
}

async fn service_worker() -> Response {
    let mut response = asset(SERVICE_WORKER, "text/javascript; charset=utf-8", "no-cache");
    response
        .headers_mut()
        .insert("service-worker-allowed", HeaderValue::from_static("/"));
    response
}

async fn app_icon_32() -> Response {
    asset(APP_ICON_32, "image/png", "public, max-age=86400")
}

async fn app_icon_192() -> Response {
    asset(APP_ICON_192, "image/png", "public, max-age=86400")
}

async fn app_icon_512() -> Response {
    asset(APP_ICON_512, "image/png", "public, max-age=86400")
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

#[derive(Deserialize)]
struct CodeServerProjectRequest {
    project: PathBuf,
}

#[derive(Deserialize)]
struct OpenFileChangeRequest {
    project: PathBuf,
    session_id: Uuid,
    #[serde(default)]
    no_project: bool,
    #[serde(default)]
    change_ids: Vec<Uuid>,
}

async fn code_server_status(
    State(state): State<ServerState>,
    Query(request): Query<CodeServerProjectRequest>,
) -> ApiResult<EditorStatus> {
    Ok(Json(
        state.inner.code_server.status_for_project(&request.project),
    ))
}

async fn start_code_server(
    State(state): State<ServerState>,
    Json(request): Json<CodeServerProjectRequest>,
) -> ApiResult<EditorStatus> {
    Ok(Json(state.inner.code_server.start(&request.project).await))
}

async fn restart_code_server(
    State(state): State<ServerState>,
    Json(request): Json<CodeServerProjectRequest>,
) -> ApiResult<EditorStatus> {
    Ok(Json(
        state.inner.code_server.restart(&request.project).await,
    ))
}

async fn open_file_change(
    State(state): State<ServerState>,
    Json(request): Json<OpenFileChangeRequest>,
) -> ApiResult<EditorStatus> {
    if request.change_ids.is_empty() {
        return Err(ApiError::message(
            StatusCode::BAD_REQUEST,
            "at least one file change is required",
        ));
    }
    let status = state.inner.code_server.start(&request.project).await;
    let instance_id = match status {
        EditorStatus::Starting { instance_id, .. } | EditorStatus::Ready { instance_id, .. } => {
            instance_id
        }
        EditorStatus::Unavailable { message } | EditorStatus::Failed { message, .. } => {
            return Err(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                error: anyhow::anyhow!(message),
            });
        }
        EditorStatus::Closed => {
            return Err(ApiError::message(
                StatusCode::SERVICE_UNAVAILABLE,
                "code-server did not start",
            ));
        }
    };
    let store = if request.no_project {
        ChangeStore::no_project_at(&state.inner.registry.data_dir()?, request.session_id)?
    } else {
        ChangeStore::new(&request.project, request.session_id)
    };
    let conversation = state
        .inner
        .conversations
        .get(request.session_id)
        .filter(|handle| {
            let snapshot = handle.snapshot();
            if request.no_project {
                snapshot.session.scope == crate::session::SessionScope::NoProject
            } else {
                snapshot.session.scope == crate::session::SessionScope::Project
                    && paths_equal(&snapshot.project_root, &request.project)
            }
        });
    let live_activities = conversation
        .as_ref()
        .map(|handle| handle.observation().activities)
        .unwrap_or_default();
    let session = conversation
        .map(|handle| handle.snapshot().session)
        .map(Ok)
        .unwrap_or_else(|| {
            SessionStore::for_project_root_in(
                (!request.no_project).then_some(request.project.as_path()),
                &state.inner.registry.data_dir()?,
            )?
            .load(Some(&request.session_id.to_string()))
        })?;
    let mut files = Vec::new();
    let mut title = "Operation changes".to_owned();
    for change_id in request.change_ids {
        let referenced_change = session
            .file_changes
            .iter()
            .find(|change| change.id == change_id)
            .cloned();
        if referenced_change.is_none()
            && !activities_reference_change(&session.activities, change_id)
            && !activities_reference_change(&live_activities, change_id)
        {
            return Err(ApiError::message(
                StatusCode::NOT_FOUND,
                "file change is not part of this session",
            ));
        }
        let change = match store.load_change(change_id) {
            Ok(change) => change,
            Err(_) => referenced_change.ok_or_else(|| {
                ApiError::message(StatusCode::NOT_FOUND, "file change is not available yet")
            })?,
        };
        let reconstructed = store.reconstruct(&change).map_err(|error| {
            let temporary = change
                .files
                .iter()
                .any(|file| matches!(file, crate::session::FileChange::Temporary(_)));
            let external = change.files.iter().any(|file| {
                matches!(file, crate::session::FileChange::Temporary(_))
                    && !file.path().starts_with(&request.project)
            });
            ApiError {
                status: StatusCode::GONE,
                error: if external {
                    anyhow::anyhow!(
                        "Historical diffs for files outside the selected project are temporary and are no longer available."
                    )
                } else if temporary && !crate::changes::git_is_available() {
                    anyhow::anyhow!(
                        "Git is unavailable. Install Git, then initialize the selected project as a Git repository to preserve historical diffs."
                    )
                } else if temporary {
                    anyhow::anyhow!(
                        "Historical diffs require Git. Initialize the project as a Git repository to preserve changes across turns."
                    )
                } else {
                    error
                },
            }
        })?;
        if reconstructed.kind == crate::session::FileChangeKind::Turn {
            title = match change.outcome {
                Some(crate::session::TurnOutcome::Cancelled) => "Changes before cancellation",
                Some(crate::session::TurnOutcome::Failed) => "Changes before error",
                _ => "Turn file changes",
            }
            .to_owned();
        }
        files.extend(reconstructed.files);
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.changed_lines));
    state.inner.code_server.enqueue(
        instance_id,
        ExtensionAction::OpenDiff {
            title,
            files: files
                .into_iter()
                .map(|file| ExtensionDiffFile {
                    path: file.path,
                    before: file.before,
                    after: file.after,
                    focus_line: file.focus_line,
                    changed_lines: file.changed_lines,
                })
                .collect(),
            focus: 0,
        },
    )?;
    Ok(Json(state.inner.code_server.status(instance_id)))
}

fn activities_reference_change(activities: &[AgentActivity], change_id: Uuid) -> bool {
    activities.iter().any(|activity| {
        activity.live_change_id == Some(change_id) || activity.change_id == Some(change_id)
    })
}

fn extension_authenticated(state: &ServerState, instance_id: Uuid, headers: &HeaderMap) -> bool {
    headers
        .get("x-codecrab-extension-token")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|token| state.inner.code_server.authenticate(instance_id, token))
}

async fn code_server_extension_handshake(
    State(state): State<ServerState>,
    AxumPath(instance_id): AxumPath<Uuid>,
    headers: HeaderMap,
) -> ApiResult<EditorStatus> {
    if !extension_authenticated(&state, instance_id, &headers) {
        return Err(ApiError::message(
            StatusCode::UNAUTHORIZED,
            "invalid extension token",
        ));
    }
    if !state.inner.code_server.handshake(instance_id) {
        return Err(ApiError::message(
            StatusCode::NOT_FOUND,
            "code-server instance not found",
        ));
    }
    Ok(Json(state.inner.code_server.status(instance_id)))
}

async fn code_server_extension_commands(
    State(state): State<ServerState>,
    AxumPath(instance_id): AxumPath<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Vec<crate::code_server::ExtensionCommand>> {
    if !extension_authenticated(&state, instance_id, &headers) {
        return Err(ApiError::message(
            StatusCode::UNAUTHORIZED,
            "invalid extension token",
        ));
    }
    Ok(Json(
        state
            .inner
            .code_server
            .take_commands(instance_id)
            .unwrap_or_default(),
    ))
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
    listeners: BoundListeners,
    tls_config: RustlsConfig,
    app: Router,
    shutdown: F,
    force_shutdown: S,
) -> Result<ShutdownOutcome>
where
    F: Future<Output = Result<()>>,
    S: Future<Output = Result<()>>,
{
    let http_handle = Handle::new();
    let https_handle = Handle::new();
    let http_server = axum_server::from_tcp(listeners.http)
        .context("cannot configure HTTP web server")?
        .handle(http_handle.clone())
        .serve(app.clone().into_make_service());
    let https_server = axum_server::from_tcp_rustls(listeners.https, tls_config)
        .context("cannot configure HTTPS web server")?
        .handle(https_handle.clone())
        .serve(app.into_make_service());
    let servers = async {
        tokio::try_join!(
            async { http_server.await.context("HTTP web server failed") },
            async { https_server.await.context("HTTPS web server failed") },
        )?;
        Ok::<(), anyhow::Error>(())
    };
    tokio::pin!(servers);
    tokio::pin!(force_shutdown);

    tokio::select! {
        result = servers.as_mut() => {
            result?;
            return Ok(ShutdownOutcome::Graceful);
        },
        signal = shutdown => signal?,
    }

    http_handle.graceful_shutdown(None);
    https_handle.graceful_shutdown(None);
    let warning_delay = tokio::time::sleep(SHUTDOWN_WARNING_DELAY);
    tokio::pin!(warning_delay);
    tokio::select! {
        result = servers.as_mut() => {
            result?;
            return Ok(ShutdownOutcome::Graceful);
        },
        signal = force_shutdown.as_mut() => {
            signal?;
            http_handle.shutdown();
            https_handle.shutdown();
            return Ok(ShutdownOutcome::Forced);
        },
        () = warning_delay.as_mut() => {}
    }

    eprintln!("{SHUTDOWN_WAITING_MESSAGE}");
    tokio::select! {
        result = servers.as_mut() => {
            result?;
            Ok(ShutdownOutcome::Graceful)
        },
        signal = force_shutdown.as_mut() => {
            signal?;
            http_handle.shutdown();
            https_handle.shutdown();
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
    use std::{fs, path::Path};

    use super::*;
    use crate::{
        config::paths_equal,
        events::{ActivityKind, ActivityStatus},
    };
    use rcgen::SanType;
    use tokio::{
        io::AsyncWriteExt,
        net::{TcpListener, TcpStream},
        sync::{Barrier, Notify, oneshot},
    };

    fn test_conversation(agent: Agent) -> ConversationHandle {
        let registry = SessionRegistry::at(agent.project_root().join("test-global-config.toml"));
        ConversationHandle::spawn(agent, registry).unwrap()
    }

    fn api_ok<T>(result: ApiResult<T>) -> T {
        match result {
            Ok(Json(value)) => value,
            Err(error) => panic!("{:#}", error.error),
        }
    }

    fn api_error<T>(result: ApiResult<T>) -> ApiError {
        match result {
            Ok(_) => panic!("expected API error"),
            Err(error) => error,
        }
    }

    #[tokio::test]
    async fn pwa_assets_have_installable_metadata_and_safe_cache_headers() {
        let manifest_response = web_manifest().await;
        assert_eq!(manifest_response.status(), StatusCode::OK);
        assert_eq!(
            manifest_response.headers()[CONTENT_TYPE],
            "application/manifest+json; charset=utf-8"
        );
        let manifest_body = axum::body::to_bytes(manifest_response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_body).unwrap();
        assert_eq!(manifest["display"], "standalone");
        assert_eq!(manifest["start_url"], "/");
        assert_eq!(manifest["icons"].as_array().unwrap().len(), 2);
        assert_eq!(manifest["icons"][0]["sizes"], "192x192");
        assert_eq!(manifest["icons"][1]["sizes"], "512x512");
        assert!(
            std::str::from_utf8(INDEX_HTML)
                .unwrap()
                .contains("href=\"/icon-32.png\"")
        );

        for (icon_response, expected_size) in [
            (app_icon_32().await, 32),
            (app_icon_192().await, 192),
            (app_icon_512().await, 512),
        ] {
            assert_eq!(icon_response.headers()[CONTENT_TYPE], "image/png");
            let icon = axum::body::to_bytes(icon_response.into_body(), 512 * 1024)
                .await
                .unwrap();
            assert_eq!(&icon[..8], b"\x89PNG\r\n\x1a\n");
            assert_eq!(
                u32::from_be_bytes(icon[16..20].try_into().unwrap()),
                expected_size
            );
            assert_eq!(
                u32::from_be_bytes(icon[20..24].try_into().unwrap()),
                expected_size
            );
            let decoded = image::load_from_memory(&icon).unwrap().to_rgba8();
            for corner in [
                (0, 0),
                (expected_size - 1, 0),
                (0, expected_size - 1),
                (expected_size - 1, expected_size - 1),
            ] {
                assert_eq!(decoded.get_pixel(corner.0, corner.1).0[3], 0);
            }
        }

        let worker_response = service_worker().await;
        assert_eq!(worker_response.status(), StatusCode::OK);
        assert_eq!(worker_response.headers()[CACHE_CONTROL], "no-cache");
        assert_eq!(worker_response.headers()["service-worker-allowed"], "/");
        let worker_body = axum::body::to_bytes(worker_response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let worker = std::str::from_utf8(&worker_body).unwrap();
        assert!(worker.contains("url.pathname.startsWith(\"/api/\")"));
        assert!(worker.contains("\"/icon-32.png\""));
        assert!(!worker.contains("icon.svg"));
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

        let value = serde_json::to_value(WebSession::persisted(session)).unwrap();
        assert_eq!(value["messages"].as_array().unwrap().len(), 1);
        assert_eq!(value["active_message_ids"].as_array().unwrap().len(), 1);
        assert_eq!(value["branch_nodes"].as_array().unwrap().len(), 1);
        assert_eq!(value["conversation"]["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(
            value["messages"][0]["content"].as_str(),
            Some("Inspect the tree")
        );
    }

    #[test]
    fn live_web_sessions_override_stale_transcript_and_activity_state() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionStore::new(root.path())
            .unwrap()
            .create("test-model".into())
            .unwrap();
        let snapshot = crate::conversation::ConversationSnapshot {
            project_root: root.path().to_path_buf(),
            session: session.clone(),
            skills: Vec::new(),
            model_catalog: Vec::new(),
        };
        let mut observation = crate::conversation::ConversationObservation {
            revision: 3,
            title: "Live delegated title".into(),
            manual_title: false,
            lifecycle: ConversationLifecycle::Running,
            active_turn_started_at: Some(chrono::Utc::now()),
            latest_event_at: chrono::Utc::now(),
            catalog_error: None,
            last_turn: None,
            visible_goal: None,
            messages: vec![crate::conversation::ObservedMessage {
                id: "user:live".into(),
                role: "user",
                sequence: None,
                created_at: chrono::Utc::now(),
                content: "Live delegated title".into(),
                partial: false,
            }],
            activity: None,
            display_messages: vec![Message::text(
                crate::provider::Role::User,
                "Live delegated title",
            )],
            activities: Vec::new(),
        };
        let activity = AgentActivity::started(
            "delegated-read".into(),
            Uuid::new_v4(),
            0,
            1,
            "read_file",
            r#"{"path":"src/main.rs"}"#,
        );
        observation.activities.push(activity);

        let value = serde_json::to_value(WebSession::live(snapshot.session, observation)).unwrap();
        assert_eq!(value["title"], "Live delegated title");
        assert_eq!(value["messages"][0]["content"], "Live delegated title");
        assert_eq!(value["activities"][0]["id"], "delegated-read");
        assert_eq!(value["activities"][0]["detail"], "src/main.rs");
    }

    #[tokio::test]
    async fn session_stream_sync_bootstraps_selected_views_without_idle_transcript_fanout() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("test-model", "http://127.0.0.1:1/v1");
        let session = SessionStore::new(&root)
            .unwrap()
            .create_for_provider(config.active_provider.clone(), "test-model".into())
            .unwrap();
        SessionStore::new(&root).unwrap().save(&session).unwrap();
        let agent = build_agent(&root, &config, false, session).unwrap();
        let conversation = test_conversation(agent);
        let state = test_state(config.clone(), root.clone(), conversation, false);
        let idle_session = SessionStore::new(&root)
            .unwrap()
            .create_for_provider(config.active_provider.clone(), "test-model".into())
            .unwrap();
        SessionStore::new(&root)
            .unwrap()
            .save(&idle_session)
            .unwrap();
        let idle_agent = build_agent(&root, &config, false, idle_session).unwrap();
        state.inner.conversations.install(idle_agent).unwrap();

        let message = session_sync_message(&state, 7).await.unwrap();
        let value = serde_json::to_value(message).unwrap();

        assert_eq!(value["type"], "sync");
        assert_eq!(value["revision"], 7);
        let project = value["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|project| project["root"].is_string())
            .unwrap();
        assert_eq!(project["sessions"].as_array().unwrap().len(), 2);
        assert_eq!(value["workers"].as_array().unwrap().len(), 2);
        assert_eq!(value["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(value["sessions"][0]["lifecycle"], "idle");
    }

    fn test_state(
        config: Config,
        root: PathBuf,
        conversation: ConversationHandle,
        oauth_logged_in: bool,
    ) -> ServerState {
        let registry = SessionRegistry::at(root.join(".test-global-config").join("config.toml"));
        test_state_with_registry(config, root, conversation, oauth_logged_in, registry)
    }

    fn test_state_with_registry(
        config: Config,
        root: PathBuf,
        conversation: ConversationHandle,
        oauth_logged_in: bool,
        registry: SessionRegistry,
    ) -> ServerState {
        let usage = UsageTracker::test(oauth_logged_in, None).unwrap();
        test_state_with_registry_and_usage(
            config,
            root,
            conversation,
            oauth_logged_in,
            registry,
            usage,
        )
    }

    fn test_state_with_registry_and_usage(
        config: Config,
        root: PathBuf,
        conversation: ConversationHandle,
        oauth_logged_in: bool,
        registry: SessionRegistry,
        usage: UsageTracker,
    ) -> ServerState {
        let session_id = conversation.snapshot().session.id;
        let coordinator = SessionCoordinator::new(
            config.clone(),
            registry.clone(),
            DebugOutput::default(),
            DiagnosticLog::stderr(),
            root.clone(),
            root.join(".test-global-config").join("AGENTS.md"),
        );
        ServerState {
            inner: Arc::new(ServerInner {
                runtime_root: root.clone(),
                coordinator,
                config: RwLock::new(config),
                registry: registry.clone(),
                debug_openai: DebugOutput::default(),
                oauth_logged_in,
                workspace_transition: Mutex::new(()),
                workspace: Mutex::new(ServerWorkspace {
                    root: root.clone(),
                    project_selected: true,
                    selected_session: Some(session_id),
                    conversation: Some(conversation.clone()),
                }),
                conversations: ConversationManager::with_handle(registry, conversation),
                catalogs: RwLock::new(HashMap::new()),
                cron: CronStore::at(
                    root.join("test-cron.json"),
                    root.join(".test-global-config").join("cron-runtime"),
                ),
                usage,
                code_server: CodeServerManager::new(None).unwrap(),
            }),
        }
    }

    #[test]
    fn observed_activity_authorizes_its_change_before_the_turn_snapshot_finishes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let session = SessionStore::new(&root)
            .unwrap()
            .create("test-model".into())
            .unwrap();
        let change_id = Uuid::new_v4();
        let mut activity = AgentActivity::started(
            "call-write".into(),
            Uuid::new_v4(),
            0,
            0,
            "write_file",
            r#"{"path":"note.txt","content":"updated"}"#,
        );
        activity.live_change_id = Some(change_id);

        assert!(session.file_changes.is_empty());
        assert!(session.activities.is_empty());
        assert!(activities_reference_change(&[activity], change_id));
        assert!(!activities_reference_change(&[], change_id));
    }

    #[test]
    fn embeds_exactly_the_three_web_assets() {
        assert!(INDEX_HTML.starts_with(b"<!doctype html>"));
        assert!(APP_JS.len() > 1_000);
        assert!(APP_CSS.len() > 1_000);
    }

    #[tokio::test]
    async fn provider_api_switches_only_the_selected_session_and_rolls_back_without_models() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut config = Config::test("old-model", "http://127.0.0.1:1/v1");
        let mut local = crate::config::ProviderConfig::test(
            "local-model".into(),
            "http://127.0.0.1:2/v1".into(),
        );
        local.fetch_models = false;
        local.model_capabilities.insert(
            "local-model".into(),
            crate::config::ModelCapabilitiesConfig::default(),
        );
        config.providers.insert("local".into(), local);
        let mut empty =
            crate::config::ProviderConfig::test("auto".into(), "http://127.0.0.1:3/v1".into());
        empty.fetch_models = false;
        config.providers.insert("empty".into(), empty);
        let session = SessionStore::new(&root)
            .unwrap()
            .create_for_provider(config.active_provider.clone(), "old-model".into())
            .unwrap();
        let session_id = session.id;
        let agent = build_agent(&root, &config, false, session).unwrap();
        let state = test_state(config, root.clone(), test_conversation(agent), false);

        let response = match set_provider(
            State(state.clone()),
            Json(ProviderSelectionRequest {
                session_id: Some(session_id),
                provider: "local".into(),
            }),
        )
        .await
        {
            Ok(response) => response.0,
            Err(error) => panic!("provider switch failed: {:#}", error.error),
        };
        let session = response.session.unwrap();
        assert_eq!(session.provider, "local");
        assert_eq!(session.model, "local-model");
        assert_eq!(
            state.inner.config.read().unwrap().active_provider,
            crate::config::DEFAULT_PROVIDER
        );
        assert_eq!(response.models[0].slug, "local-model");

        let error = match set_provider(
            State(state.clone()),
            Json(ProviderSelectionRequest {
                session_id: Some(session_id),
                provider: "empty".into(),
            }),
        )
        .await
        {
            Ok(_) => panic!("a provider without models must be rejected"),
            Err(error) => error,
        };
        assert_eq!(
            format!("{:#}", error.error),
            crate::agent::NO_PROVIDER_MODELS_MESSAGE
        );
        let snapshot = selected_conversation(&state, Some(session_id))
            .await
            .unwrap()
            .snapshot();
        assert_eq!(snapshot.session.provider, "local");
        assert_eq!(snapshot.session.model, "local-model");
        assert_eq!(snapshot.model_catalog[0].slug, "local-model");
    }

    #[tokio::test]
    async fn cron_api_crud_uses_the_shared_validated_store() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("test-model", "http://127.0.0.1:1/v1");
        let session = SessionStore::new(&root)
            .unwrap()
            .create_for_provider(config.active_provider.clone(), "test-model".into())
            .unwrap();
        let agent = build_agent(&root, &config, false, session).unwrap();
        let state = test_state(config, root.clone(), test_conversation(agent), false);
        let job = CronJob {
            schedule: "@daily".into(),
            enabled: true,
            project: root,
            prompt: "Run tests".into(),
            provider: "test".into(),
            model: "test-model".into(),
            reasoning: None,
            speed: None,
            timezone: Some("UTC".into()),
            overlap: crate::cron::OverlapPolicy::Skip,
            timeout_seconds: None,
            source_session_id: None,
        };

        let created = upsert_cron_job(
            State(state.clone()),
            Json(CronJobRequest {
                id: "daily-tests".into(),
                job,
            }),
        )
        .await
        .unwrap_or_else(|error| panic!("{:#}", error.error))
        .0;
        assert_eq!(created.jobs[0].id, "daily-tests");

        let paused = set_cron_job_enabled(
            State(state.clone()),
            Json(CronEnabledRequest {
                id: "daily-tests".into(),
                enabled: false,
            }),
        )
        .await
        .unwrap_or_else(|error| panic!("{:#}", error.error))
        .0;
        assert!(!paused.document.jobs["daily-tests"].enabled);

        let deleted = delete_cron_job(
            State(state),
            Json(CronJobIdRequest {
                id: "daily-tests".into(),
            }),
        )
        .await
        .unwrap_or_else(|error| panic!("{:#}", error.error))
        .0;
        assert!(deleted.jobs.is_empty());
    }

    #[tokio::test]
    async fn invalid_cron_json_does_not_break_the_rest_of_web_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("test-model", "http://127.0.0.1:1/v1");
        let session = SessionStore::new(&root)
            .unwrap()
            .create_for_provider(config.active_provider.clone(), "test-model".into())
            .unwrap();
        let agent = build_agent(&root, &config, false, session).unwrap();
        let state = test_state(config, root.clone(), test_conversation(agent), false);
        fs::write(root.join("test-cron.json"), b"{invalid").unwrap();

        let response = snapshot(&state).await.unwrap();

        assert!(response.cron.is_none());
        assert!(response.cron_error.unwrap().contains("not valid cron JSON"));
        assert_eq!(
            response.commands,
            builtin_command_names().collect::<Vec<_>>()
        );
        assert!(paths_equal(
            Path::new(response.project.as_ref().unwrap()),
            &root
        ));
    }

    #[test]
    fn https_certificate_parameters_include_local_and_configured_sans() {
        let params = tls_certificate_params("codecrab.local").unwrap();
        assert!(
            params
                .subject_alt_names
                .iter()
                .any(|san| matches!(san, SanType::DnsName(name) if name.as_str() == "localhost"))
        );
        assert!(
            params.subject_alt_names.iter().any(
                |san| matches!(san, SanType::DnsName(name) if name.as_str() == "codecrab.local")
            )
        );
        assert!(params.subject_alt_names.iter().any(
            |san| matches!(san, SanType::IpAddress(ip) if *ip == IpAddr::V4(Ipv4Addr::LOCALHOST))
        ));
        assert!(params.subject_alt_names.iter().any(
            |san| matches!(san, SanType::IpAddress(ip) if *ip == IpAddr::V6(Ipv6Addr::LOCALHOST))
        ));

        let concrete_ip = "192.0.2.10".parse::<IpAddr>().unwrap();
        let params = tls_certificate_params(&concrete_ip.to_string()).unwrap();
        assert!(
            params
                .subject_alt_names
                .contains(&SanType::IpAddress(concrete_ip))
        );
        assert!(!tls_subject_alt_names("0.0.0.0").contains(&"0.0.0.0".to_owned()));
        assert!(!tls_subject_alt_names("::").contains(&"::".to_owned()));
    }

    #[test]
    fn https_certificate_and_key_are_regenerated_ephemerally() {
        let first = generate_tls_material("127.0.0.1").unwrap();
        let second = generate_tls_material("127.0.0.1").unwrap();

        assert!(!first.certificate_der.is_empty());
        assert!(!first.private_key_der.is_empty());
        assert_ne!(first.certificate_der, second.certificate_der);
        assert_ne!(first.private_key_der, second.private_key_der);
    }

    #[tokio::test]
    async fn invalid_tls_material_reports_a_contextual_setup_error() {
        let error = tls_config(TlsMaterial {
            certificate_der: vec![0],
            private_key_der: vec![0],
        })
        .await
        .unwrap_err();

        assert!(format!("{error:#}").contains("cannot configure HTTPS TLS"));
    }

    #[tokio::test]
    async fn https_bind_failure_drops_the_already_bound_http_listener() {
        let unavailable_https = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let https_port = unavailable_https.local_addr().unwrap().port();
        let available_http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = available_http.local_addr().unwrap().port();
        drop(available_http);

        let error = bind_listeners("127.0.0.1", http_port, https_port)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(&format!(
            "cannot bind HTTPS web server to 127.0.0.1:{https_port}"
        )));

        TcpListener::bind(("127.0.0.1", http_port))
            .await
            .expect("the HTTP listener must be dropped when HTTPS binding fails");
    }

    #[tokio::test]
    async fn http_and_https_serve_the_same_frontend_api_and_state() {
        let provider_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_address = provider_listener.local_addr().unwrap();
        let provider_server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = provider_listener.accept().await.unwrap();
                let _request = crate::test_support::read_http_request(&mut socket).await;
                let body = json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "Finished over both schemes."
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
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("model", format!("http://{provider_address}/v1"));
        let session = SessionStore::new(&root)
            .unwrap()
            .create_for_provider(config.active_provider.clone(), "model".into())
            .unwrap();
        let session_id = session.id;
        let agent = build_agent(&root, &config, false, session).unwrap();
        let state = test_state(config, root, test_conversation(agent), false);
        let app = server_app(state);
        let listeners = bind_listeners("127.0.0.1", 0, 0).await.unwrap();
        let http_origin = display_origin("http", listeners.http_address);
        let https_origin = display_origin("https", listeners.https_address);
        let config = tls_config(generate_tls_material("127.0.0.1").unwrap())
            .await
            .unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (_force_tx, force_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until_shutdown(
            listeners,
            config,
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
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .no_proxy()
            .build()
            .unwrap();

        for origin in [&http_origin, &https_origin] {
            let frontend = client.get(format!("{origin}/")).send().await.unwrap();
            assert!(frontend.status().is_success());
            assert!(
                frontend
                    .text()
                    .await
                    .unwrap()
                    .starts_with("<!doctype html>")
            );

            let health = client
                .get(format!("{origin}/api/health"))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap();
            assert_eq!(health["ok"], true);

            let state = client
                .get(format!("{origin}/api/state"))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap();
            assert_eq!(state["session"]["id"], json!(session_id));

            let chat = client
                .post(format!("{origin}/api/chat"))
                .json(&json!({
                    "session_id": session_id,
                    "prompt": format!("Reply through {origin}"),
                    "continuation": false,
                    "edit_node_id": null
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(
                chat.headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some("application/x-ndjson; charset=utf-8")
            );
            let stream = chat.text().await.unwrap();
            assert!(stream.contains("Finished over both schemes."));
            assert!(stream.contains("\"type\":\"done\""));
        }

        shutdown_tx.send(()).unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("both listeners did not stop gracefully")
            .unwrap()
            .unwrap();
        assert_eq!(outcome, ShutdownOutcome::Graceful);
        provider_server.await.unwrap();
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
    async fn process_api_lists_views_stops_and_guards_session_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("model", "http://127.0.0.1:1/v1");
        let store = SessionStore::new(&root).unwrap();
        let session = store
            .create_for_provider(config.active_provider.clone(), "model".into())
            .unwrap();
        let session_id = session.id;
        store.save(&session).unwrap();
        let agent = build_agent(&root, &config, false, session).unwrap();
        let terminals = agent.terminal_manager();
        let command = if cfg!(windows) {
            "$value = Read-Host 'Value'; Write-Output $value"
        } else {
            "printf 'Value: '; IFS= read value; printf '%s\\n' \"$value\""
        };
        let first_manager = terminals.clone();
        let second_manager = terminals.clone();
        let (first, second) =
            tokio::join!(first_manager.shell(command), second_manager.shell(command));
        let first_id = first.unwrap()["terminal_id"].as_str().unwrap().to_owned();
        let second_id = second.unwrap()["terminal_id"].as_str().unwrap().to_owned();
        terminals
            .set_origin_activity(&first_id, "call-shell-first")
            .unwrap();
        terminals
            .set_origin_activity(&second_id, "call-shell-second")
            .unwrap();

        let state = test_state(config, root.clone(), test_conversation(agent), false);
        let active_terminal_count = |response: &StateResponse| {
            response
                .projects
                .iter()
                .flat_map(|project| &project.sessions)
                .find(|session| session.id == session_id)
                .unwrap()
                .active_terminal_count
        };
        let initial_state = snapshot_for(&state, Some(session_id)).await.unwrap();
        assert_eq!(active_terminal_count(&initial_state), 2);
        let processes = api_ok(
            list_processes(
                State(state.clone()),
                Query(ProcessRequest {
                    session_id: Some(session_id),
                }),
            )
            .await,
        );
        assert_eq!(processes.len(), 2);
        assert!(processes.iter().any(|process| {
            process.terminal_id == first_id
                && process.origin_activity_id.as_deref() == Some("call-shell-first")
        }));

        let output = api_ok(
            process_output(
                State(state.clone()),
                AxumPath(first_id.clone()),
                Query(ProcessRequest {
                    session_id: Some(session_id),
                }),
            )
            .await,
        );
        assert_eq!(output.process_state, TerminalProcessState::Running);
        assert_eq!(
            output.origin_activity_id.as_deref(),
            Some("call-shell-first")
        );

        let stopped = api_ok(
            stop_process(
                State(state.clone()),
                Json(StopProcessRequest {
                    session_id: Some(session_id),
                    terminal_id: first_id,
                }),
            )
            .await,
        );
        assert_eq!(stopped.process_state, TerminalProcessState::Closed);
        assert_eq!(
            api_ok(
                list_processes(
                    State(state.clone()),
                    Query(ProcessRequest {
                        session_id: Some(session_id),
                    }),
                )
                .await
            )
            .len(),
            1
        );
        let updated_state = snapshot_for(&state, Some(session_id)).await.unwrap();
        assert_eq!(active_terminal_count(&updated_state), 1);

        let rejected = api_error(
            delete_session(
                State(state.clone()),
                Json(SessionRequest {
                    project: Some(root.clone()),
                    id: session_id.to_string(),
                    stop_processes: false,
                }),
            )
            .await,
        );
        assert_eq!(rejected.status, StatusCode::CONFLICT);
        assert!(store.load(Some(&session_id.to_string())).is_ok());
        assert_eq!(terminals.running_records().len(), 1);

        api_ok(
            delete_session(
                State(state),
                Json(SessionRequest {
                    project: Some(root),
                    id: session_id.to_string(),
                    stop_processes: true,
                }),
            )
            .await,
        );
        assert!(terminals.running_records().is_empty());
        assert!(store.load(Some(&session_id.to_string())).is_err());
        assert_eq!(
            terminals.output(&second_id).unwrap().process_state,
            TerminalProcessState::Closed
        );
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
        let listeners = bind_listeners("127.0.0.1", 0, 0).await.unwrap();
        let address = listeners.http_address;
        let config = tls_config(generate_tls_material("127.0.0.1").unwrap())
            .await
            .unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (force_tx, force_rx) = oneshot::channel();
        let mut server = tokio::spawn(serve_until_shutdown(
            listeners,
            config,
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
        assert!(SHUTDOWN_WAITING_MESSAGE.contains("active HTTP/HTTPS requests"));
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
            change_id: None,
            live_change_id: None,
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
                attachments: Vec::new(),
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
                attachments: Vec::new(),
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
                attachments: Vec::new(),
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
                attachments: Vec::new(),
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
    async fn usage_api_refreshes_and_redeems_through_shared_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let provider_server = tokio::spawn(async move {
            let responses = [
                r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":37,"limit_window_seconds":604800,"reset_at":1786826526}},"rate_limit_reset_credits":{"available_count":1,"applicable_available_count":1}}"#,
                r#"{"credits":[{"id":"credit-1","reset_type":"codex_rate_limits","status":"available","granted_at":"2026-06-17T00:00:00Z","expires_at":null}],"available_count":1}"#,
                r#"{"code":"reset","windows_reset":1}"#,
                r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":0,"limit_window_seconds":604800,"reset_at":1787431326}},"rate_limit_reset_credits":{"available_count":0,"applicable_available_count":0}}"#,
            ];
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.push(crate::test_support::read_http_request(&mut socket).await);
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
            }
            requests
        });

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut config = Config::test("test-model", crate::config::OFFICIAL_OPENAI_BASE_URL);
        config
            .providers
            .get_mut(&config.active_provider)
            .unwrap()
            .auth = "oauth".into();
        let session = SessionStore::new(&root)
            .unwrap()
            .create("test-model".into())
            .unwrap();
        let session_id = session.id;
        let agent = build_agent(&root, &config, false, session).unwrap();
        let registry = SessionRegistry::at(root.join("test-global-config.toml"));
        let usage = UsageTracker::test(true, Some(format!("http://{address}"))).unwrap();
        let state = test_state_with_registry_and_usage(
            config,
            root,
            test_conversation(agent),
            true,
            registry,
            usage,
        );

        let refreshed = get_usage(
            State(state.clone()),
            Query(UsageRequest {
                session_id: Some(session_id),
                coalesce: false,
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;
        assert!(refreshed.available);
        assert!(!refreshed.stale);
        assert!(refreshed.can_reset);
        assert_eq!(
            refreshed.snapshot.unwrap().windows[0].remaining_percent,
            63.0
        );

        let reset = reset_usage(
            State(state),
            Json(ResetUsageRequest {
                session_id: Some(session_id),
                idempotency_key: "request-123".into(),
                credit_id: None,
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;
        assert_eq!(reset.outcome, crate::account_usage::ResetOutcome::Reset);
        assert_eq!(reset.windows_reset, 1);
        assert!(!reset.usage.can_reset);
        assert_eq!(
            reset.usage.snapshot.unwrap().windows[0].remaining_percent,
            100.0
        );

        let requests = provider_server.await.unwrap();
        let reset_request = String::from_utf8(requests[2].clone()).unwrap();
        assert!(reset_request.starts_with("POST /wham/rate-limit-reset-credits/consume"));
        assert!(reset_request.contains(r#""redeem_request_id":"request-123""#));
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
        assert!(!snapshot(&state).await.unwrap().usage.available);

        let slash = completions(
            State(state.clone()),
            Json(CompletionRequest {
                request_id: 1,
                session_id: None,
                before_cursor: "/".into(),
                after_cursor: String::new(),
                skill_refresh_id: None,
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
        assert!(slash.items.iter().all(|item| item.name != "usage"));

        let files = completions(
            State(state.clone()),
            Json(CompletionRequest {
                request_id: 2,
                session_id: None,
                before_cursor: "@".into(),
                after_cursor: String::new(),
                skill_refresh_id: None,
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
                skill_refresh_id: None,
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
            State(state.clone()),
            Json(CompletionRequest {
                request_id: 9,
                session_id: None,
                before_cursor: "@config".into(),
                after_cursor: String::new(),
                skill_refresh_id: None,
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

        let decorated = completions(
            State(state),
            Json(CompletionRequest {
                request_id: 10,
                session_id: None,
                before_cursor: "Use /review-rust and /missing".into(),
                after_cursor: String::new(),
                skill_refresh_id: None,
            }),
        )
        .await
        .ok()
        .unwrap()
        .0
        .unwrap();
        assert!(decorated.items.is_empty());
        assert_eq!(
            decorated
                .segments
                .iter()
                .filter_map(|segment| segment.kind)
                .collect::<Vec<_>>(),
            vec![
                crate::completion::ComposerDecorationKind::Skill,
                crate::completion::ComposerDecorationKind::Invalid,
            ]
        );
    }

    #[tokio::test]
    async fn slash_completion_refreshes_skills_once_per_opening() {
        async fn slash(
            state: &ServerState,
            request_id: u64,
            before_cursor: &str,
            skill_refresh_id: Uuid,
        ) -> CompletionResponse {
            completions(
                State(state.clone()),
                Json(CompletionRequest {
                    request_id,
                    session_id: None,
                    before_cursor: before_cursor.into(),
                    after_cursor: String::new(),
                    skill_refresh_id: Some(skill_refresh_id),
                }),
            )
            .await
            .ok()
            .unwrap()
            .0
            .unwrap()
        }

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
        let state = test_state(config, root.clone(), test_conversation(agent), false);

        let skill = root.join(".agents/skills/review-rust");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: review-rust\ndescription: Review Rust changes.\n---\nReview the code.",
        )
        .unwrap();

        let first_opening = Uuid::new_v4();
        let opened = slash(&state, 1, "/", first_opening).await;
        assert!(opened.slash_context);
        assert!(opened.items.iter().any(|item| item.name == "review-rust"));

        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: review-rust\ndescription: Review updated Rust changes.\n---\nReview the updated code.",
        )
        .unwrap();
        let continued = slash(&state, 2, "/r", first_opening).await;
        let stale_skill = continued
            .items
            .iter()
            .find(|item| item.name == "review-rust")
            .unwrap();
        assert_eq!(stale_skill.description, "Review Rust changes.");

        let refreshed = slash(&state, 3, "/", Uuid::new_v4()).await;
        let refreshed_skill = refreshed
            .items
            .iter()
            .find(|item| item.name == "review-rust")
            .unwrap();
        assert_eq!(refreshed_skill.description, "Review updated Rust changes.");

        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: different-skill\ndescription: Mismatched directory.\n---\nInvalid instructions.",
        )
        .unwrap();
        let invalid = slash(&state, 4, "/", Uuid::new_v4()).await;
        assert!(invalid.items.iter().all(|item| item.name != "review-rust"));

        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: review-rust\ndescription: Restored Rust review.\n---\nRestored instructions.",
        )
        .unwrap();
        let restored_opening = Uuid::new_v4();
        let restored = slash(&state, 5, "/", restored_opening).await;
        assert!(restored.items.iter().any(|item| item.name == "review-rust"));

        std::fs::remove_file(skill.join("SKILL.md")).unwrap();
        let continued = slash(&state, 6, "/r", restored_opening).await;
        assert!(
            continued
                .items
                .iter()
                .any(|item| item.name == "review-rust")
        );
        let deleted_opening = Uuid::new_v4();
        let deleted = slash(&state, 7, "/", deleted_opening).await;
        assert!(deleted.items.iter().all(|item| item.name != "review-rust"));

        let empty_context = slash(&state, 8, "Please /zzz", deleted_opening).await;
        assert!(empty_context.slash_context);
        assert!(empty_context.items.is_empty());
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
                stop_processes: false,
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;

        assert_eq!(response.session.as_ref().unwrap().id, other.id);
        assert!(response.projects.iter().any(|project| {
            project
                .root
                .as_deref()
                .is_some_and(|root| paths_equal(root, &other_root))
        }));
        assert!(current_store.list().unwrap().is_empty());
        assert!(state.inner.conversations.get(current_id).is_none());
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
                skill_refresh_id: None,
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
        let initial_session_id = session.id;
        SessionStore::new(&current).unwrap().save(&session).unwrap();
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
        assert!(
            SessionStore::new(&current)
                .unwrap()
                .load(Some(&initial_session_id.to_string()))
                .is_err()
        );
        assert!(paths_equal(
            &state.inner.workspace.lock().await.root,
            &empty
        ));
        assert!(response.projects.iter().any(|project| {
            project
                .root
                .as_deref()
                .is_some_and(|root| paths_equal(root, &empty))
                && project.sessions.is_empty()
        }));
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
        let initial_session_id = session.id;
        SessionStore::new(&current).unwrap().save(&session).unwrap();
        let agent = build_agent(&current, &config, false, session).unwrap();
        let state = test_state(config, current.clone(), test_conversation(agent), false);

        let response = new_session(
            State(state.clone()),
            Json(NewSessionRequest {
                project: Some(target.clone()),
                no_project: false,
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;

        let created = response.session.unwrap();
        assert!(
            SessionStore::new(&current)
                .unwrap()
                .load(Some(&initial_session_id.to_string()))
                .is_err()
        );
        assert!(paths_equal(
            &state.inner.workspace.lock().await.root,
            &target
        ));
        assert_eq!(
            SessionStore::new(&target).unwrap().list().unwrap()[0].id,
            created.id
        );
        assert!(response.projects.iter().any(|project| {
            project
                .root
                .as_deref()
                .is_some_and(|root| paths_equal(root, &target))
        }));
    }

    #[tokio::test]
    async fn new_web_session_can_enter_no_project_mode() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let session = SessionStore::new(&root)
            .unwrap()
            .create("model".into())
            .unwrap();
        let agent = build_agent(&root, &config, false, session).unwrap();
        let state = test_state(config, root.clone(), test_conversation(agent), false);
        {
            let mut workspace = state.inner.workspace.lock().await;
            workspace.project_selected = false;
            workspace.selected_session = None;
            workspace.conversation = None;
        }
        let neutral = snapshot(&state).await.unwrap();
        assert_eq!(neutral.project, None);
        assert!(neutral.session.is_none());

        let response = new_session(
            State(state.clone()),
            Json(NewSessionRequest {
                project: None,
                no_project: true,
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;

        let created = response.session.unwrap();
        assert_eq!(created.scope, crate::session::SessionScope::NoProject);
        assert_eq!(response.project, None);
        assert!(response.projects[0].root.is_none());
        assert!(
            response.projects[0]
                .sessions
                .iter()
                .any(|session| session.id == created.id)
        );
        assert!(paths_equal(&state.inner.workspace.lock().await.root, &root));
    }

    #[tokio::test]
    async fn deleting_the_active_web_session_selects_the_next_or_leaves_none() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let store = SessionStore::new(&root).unwrap();
        let now = chrono::Utc::now();
        let mut newest = store.create("newest-model".into()).unwrap();
        newest.created_at = now;
        newest.updated_at = now - chrono::Duration::minutes(1);
        let newest_id = newest.id;
        store.save(&newest).unwrap();
        let mut next = store.create("next-model".into()).unwrap();
        next.created_at = now - chrono::Duration::minutes(2);
        next.updated_at = now - chrono::Duration::minutes(3);
        next.title = "Next saved session".into();
        next.messages.push(crate::provider::Message::text(
            crate::provider::Role::User,
            "next context",
        ));
        let next_id = next.id;
        store.save(&next).unwrap();
        let mut active = store.create("selected-model".into()).unwrap();
        active.created_at = now - chrono::Duration::minutes(1);
        active.updated_at = now;
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
                stop_processes: false,
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
                stop_processes: false,
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;
        assert_eq!(response.session.as_ref().unwrap().id, newest_id);

        let response = delete_session(
            State(state.clone()),
            Json(SessionRequest {
                project: Some(root.clone()),
                id: newest_id.to_string(),
                stop_processes: false,
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

    #[tokio::test]
    async fn session_metadata_endpoint_updates_the_worker_and_persisted_catalog() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::default();
        let store = SessionStore::new(&root).unwrap();
        let session = store.create("test-model".into()).unwrap();
        let id = session.id;
        let updated_at = session.updated_at;
        store.save(&session).unwrap();
        let agent = build_agent(&root, &config, false, session).unwrap();
        let state = test_state(config, root.clone(), test_conversation(agent), false);

        for request in [
            SessionMetadataRequest {
                project: Some(root.clone()),
                id: id.to_string(),
                title: Some("  Manual title  ".into()),
                pinned: None,
                archived: None,
            },
            SessionMetadataRequest {
                project: Some(root.clone()),
                id: id.to_string(),
                title: None,
                pinned: Some(true),
                archived: None,
            },
            SessionMetadataRequest {
                project: Some(root.clone()),
                id: id.to_string(),
                title: None,
                pinned: None,
                archived: Some(true),
            },
        ] {
            api_ok(update_session_metadata(State(state.clone()), Json(request)).await);
        }

        let persisted = store.load(Some(&id.to_string())).unwrap();
        assert_eq!(persisted.title, "Manual title");
        assert!(persisted.manual_title);
        assert!(persisted.pinned_at.is_some());
        assert!(persisted.archived_at.is_some());
        assert_eq!(persisted.updated_at, updated_at);
        let projects = live_session_projects(&root, &state.inner).unwrap();
        let project = projects
            .iter()
            .find(|project| {
                project
                    .root
                    .as_deref()
                    .is_some_and(|project_root| paths_equal(project_root, &root))
            })
            .unwrap();
        assert!(project.active_sessions().is_empty());
        assert!(project.pinned_sessions().is_empty());
        assert_eq!(project.archived_sessions()[0].id, id);
    }
}
