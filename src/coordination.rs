use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock, Weak},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::SecondsFormat;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    agent::Agent,
    config::{Config, SessionRegistry, normalized_root, paths_equal},
    conversation::{
        ConversationHandle, ConversationLifecycle, ConversationManager, ConversationObservation,
    },
    diagnostics::{DebugOutput, DiagnosticLog},
    provider::{OpenAiCompatible, Role},
    session::{Session, SessionStore, TurnOutcome, list_session_projects},
    skills::SkillRegistry,
    tools::ToolBox,
};

#[derive(Clone)]
pub(crate) struct SessionCoordinator {
    inner: Arc<SessionCoordinatorInner>,
}

struct SessionCoordinatorInner {
    manager: ConversationManager,
    config: RwLock<Config>,
    registry: SessionRegistry,
    debug_openai: DebugOutput,
    diagnostics: DiagnosticLog,
    global_instructions_path: PathBuf,
}

#[derive(Clone)]
pub(crate) struct SessionControl {
    coordinator: Weak<SessionCoordinatorInner>,
    caller_session_id: Uuid,
    caller_project_root: PathBuf,
}

impl SessionCoordinator {
    pub(crate) fn new(
        config: Config,
        registry: SessionRegistry,
        debug_openai: DebugOutput,
        diagnostics: DiagnosticLog,
        global_instructions_path: PathBuf,
    ) -> Self {
        Self {
            inner: Arc::new(SessionCoordinatorInner {
                manager: ConversationManager::new(registry.clone()),
                config: RwLock::new(config),
                registry,
                debug_openai,
                diagnostics,
                global_instructions_path,
            }),
        }
    }

    pub(crate) fn manager(&self) -> ConversationManager {
        self.inner.manager.clone()
    }

    pub(crate) fn update_config(&self, config: Config) {
        *self
            .inner
            .config
            .write()
            .expect("session coordinator config lock poisoned") = config;
    }

    pub(crate) fn build_agent(&self, root: &Path, session: Session) -> Result<Agent> {
        let config = self
            .inner
            .config
            .read()
            .expect("session coordinator config lock poisoned")
            .clone();
        let mut provider = OpenAiCompatible::new(&config, &session.provider)?;
        provider.set_debug_openai(self.inner.debug_openai.clone());
        let control = SessionControl {
            coordinator: Arc::downgrade(&self.inner),
            caller_session_id: session.id,
            caller_project_root: normalized_root(root),
        };
        Agent::new(
            provider,
            ToolBox::with_session_control(normalized_root(root), config.shell.clone(), control),
            SkillRegistry::discover(root),
            session,
            self.inner.global_instructions_path.clone(),
            self.inner.diagnostics.clone(),
        )
    }

    pub(crate) fn install(&self, agent: Agent) -> Result<ConversationHandle> {
        self.inner.manager.install(agent)
    }

    pub(crate) fn create_session(&self, root: &Path) -> Result<Session> {
        let config = self
            .inner
            .config
            .read()
            .expect("session coordinator config lock poisoned");
        let provider = config.provider(&config.active_provider)?;
        SessionStore::new(root)?
            .create_for_provider(config.active_provider.clone(), provider.model.clone())
    }

    fn locate_session(&self, id: Uuid, caller_root: &Path) -> Result<(PathBuf, Session)> {
        if let Some(handle) = self.inner.manager.get(id) {
            let snapshot = handle.snapshot();
            return Ok((snapshot.project_root, snapshot.session));
        }
        let projects = list_session_projects(caller_root, &self.inner.registry)?;
        for project in projects {
            if project.sessions.iter().any(|session| session.id == id) {
                let session = SessionStore::new(&project.root)?.load(Some(&id.to_string()))?;
                return Ok((project.root, session));
            }
        }
        anyhow::bail!("session {id} does not exist")
    }

    fn get_or_resume(&self, id: Uuid, caller_root: &Path) -> Result<ConversationHandle> {
        if let Some(handle) = self.inner.manager.get(id) {
            return Ok(handle);
        }
        let (root, session) = self.locate_session(id, caller_root)?;
        let agent = self.build_agent(&root, session)?;
        self.install(agent)
    }

    async fn create_child(
        &self,
        caller_session_id: Uuid,
        caller_root: &Path,
        args: &Value,
    ) -> Result<Value> {
        let prompt = required_nonempty(args, "prompt")?;
        let root = match args.get("project").and_then(Value::as_str) {
            Some(project) => {
                let root = normalized_root(&caller_root.join(project));
                if !root.is_dir() {
                    anyhow::bail!("project directory does not exist: {}", root.display());
                }
                root
            }
            None => normalized_root(caller_root),
        };
        let mut session = self.create_session(&root)?;
        session.parent_session_id = Some(caller_session_id);
        let session_id = session.id;
        let agent = self.build_agent(&root, session)?;
        SessionStore::new(&root)?.save(agent.session())?;
        self.inner.registry.register(&root)?;
        let handle = self.install(agent)?;
        handle.start_new_session_turn(prompt.to_owned())?;
        let observation = handle.observation();
        Ok(json!({
            "session_id": session_id,
            "project_root": root,
            "lifecycle": observation.lifecycle,
            "observation_revision": observation.revision,
            "catalog_warning": observation.catalog_error,
        }))
    }

    fn list_sessions(
        &self,
        caller_session_id: Uuid,
        caller_root: &Path,
        args: &Value,
    ) -> Result<Value> {
        let include_current = args
            .get("include_current")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let all_projects = args
            .get("all_projects")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let explicit_project = args.get("project").and_then(Value::as_str);
        let projects = if all_projects {
            list_session_projects(caller_root, &self.inner.registry)?
        } else {
            let root = explicit_project
                .map(|project| normalized_root(&caller_root.join(project)))
                .unwrap_or_else(|| normalized_root(caller_root));
            if !root.is_dir() {
                anyhow::bail!("project directory does not exist: {}", root.display());
            }
            list_session_projects(&root, &self.inner.registry)?
                .into_iter()
                .filter(|project| paths_equal(&project.root, &root))
                .collect()
        };
        let mut sessions = Vec::new();
        for project in projects {
            let store = SessionStore::new(&project.root)?;
            for summary in project.sessions {
                if !include_current && summary.id == caller_session_id {
                    continue;
                }
                let session = store.load(Some(&summary.id.to_string()))?;
                let live = self.inner.manager.get(session.id);
                let lifecycle = live
                    .as_ref()
                    .map(ConversationHandle::lifecycle)
                    .unwrap_or(ConversationLifecycle::Idle);
                let revision = live
                    .as_ref()
                    .map(|handle| handle.observation().revision)
                    .unwrap_or(0);
                let title = live
                    .as_ref()
                    .map(|handle| handle.observation().title)
                    .unwrap_or_else(|| session.title.clone());
                sessions.push(json!({
                    "session_id": session.id,
                    "title": title,
                    "project_root": project.root,
                    "parent_session_id": session.parent_session_id,
                    "live": live.is_some(),
                    "lifecycle": lifecycle,
                    "observation_revision": revision,
                    "updated_at": session.updated_at,
                    "current": session.id == caller_session_id,
                }));
            }
        }
        Ok(json!({"sessions": sessions}))
    }

    fn statuses(&self, caller_root: &Path, args: &Value) -> Result<Value> {
        let ids = required_ids(args, "session_ids")?;
        let statuses = ids
            .into_iter()
            .map(|id| self.status(id, caller_root))
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({"sessions": statuses}))
    }

    fn status(&self, id: Uuid, caller_root: &Path) -> Result<Value> {
        let (root, session) = self.locate_session(id, caller_root)?;
        let live = self.inner.manager.get(id);
        let observation = live.as_ref().map(ConversationHandle::observation);
        Ok(status_value(
            &root,
            &session,
            observation.as_ref(),
            live.is_some(),
        ))
    }

    fn messages(&self, caller_root: &Path, args: &Value) -> Result<Value> {
        let id = required_id(args, "session_id")?;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 100) as usize;
        let (root, session) = self.locate_session(id, caller_root)?;
        let observation = self
            .inner
            .manager
            .get(id)
            .map(|handle| handle.observation())
            .unwrap_or_else(|| persisted_observation(&session));
        let (cursor_revision, mut offset) = args
            .get("cursor")
            .and_then(Value::as_str)
            .map(parse_cursor)
            .transpose()?
            .unwrap_or((observation.revision, 0));
        if cursor_revision < observation.revision && offset == observation.messages.len() {
            offset = offset.saturating_sub(1);
        }
        let end = offset.saturating_add(limit).min(observation.messages.len());
        let messages = observation
            .messages
            .get(offset..end)
            .unwrap_or_default()
            .to_vec();
        let next_cursor = (end < observation.messages.len())
            .then(|| format!("v1:{}:{end}", observation.revision));
        let cursor = format!("v1:{}:{end}", observation.revision);
        Ok(json!({
            "session_id": id,
            "project_root": root,
            "observation_revision": observation.revision,
            "messages": messages,
            "cursor": cursor,
            "next_cursor": next_cursor,
            "has_more": end < observation.messages.len(),
        }))
    }

    fn send(&self, caller_session_id: Uuid, caller_root: &Path, args: &Value) -> Result<Value> {
        let id = required_id(args, "session_id")?;
        reject_self(caller_session_id, id, "session_send")?;
        let prompt = required_nonempty(args, "prompt")?;
        let handle = self.get_or_resume(id, caller_root)?;
        handle.start_turn(prompt.to_owned(), None)?;
        Ok(json!({
            "session_id": id,
            "accepted": true,
            "lifecycle": handle.lifecycle(),
            "observation_revision": handle.observation().revision,
        }))
    }

    fn stop(&self, caller_session_id: Uuid, caller_root: &Path, args: &Value) -> Result<Value> {
        let id = required_id(args, "session_id")?;
        reject_self(caller_session_id, id, "session_stop")?;
        let handle = self.get_or_resume(id, caller_root)?;
        let requested = handle.cancel();
        Ok(json!({
            "session_id": id,
            "cancellation_requested": requested,
            "lifecycle": handle.lifecycle(),
            "observation_revision": handle.observation().revision,
        }))
    }

    async fn wait(
        &self,
        caller_session_id: Uuid,
        caller_root: &Path,
        args: &Value,
    ) -> Result<Value> {
        let targets = args
            .get("targets")
            .and_then(Value::as_array)
            .context("missing array argument \"targets\"")?;
        if targets.is_empty() || targets.len() > 8 {
            anyhow::bail!("targets must contain between 1 and 8 sessions");
        }
        let mut parsed = Vec::with_capacity(targets.len());
        for target in targets {
            let id = required_id(target, "session_id")?;
            reject_self(caller_session_id, id, "session_wait")?;
            let after = target
                .get("after_revision")
                .and_then(Value::as_u64)
                .context("missing integer argument \"after_revision\"")?;
            self.locate_session(id, caller_root)?;
            parsed.push((id, after));
        }
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000)
            .clamp(1, 60_000);
        let mut changes = self.inner.manager.observation_hub().subscribe();
        let wait = async {
            loop {
                let statuses = parsed
                    .iter()
                    .map(|(id, _)| self.status(*id, caller_root))
                    .collect::<Result<Vec<_>>>()?;
                let changed = statuses.iter().zip(&parsed).any(|(status, (_, after))| {
                    status["observation_revision"]
                        .as_u64()
                        .is_some_and(|revision| revision > *after)
                });
                if changed {
                    return Ok::<_, anyhow::Error>(statuses);
                }
                changes
                    .changed()
                    .await
                    .context("session observation feed closed")?;
            }
        };
        match tokio::time::timeout(Duration::from_millis(timeout_ms), wait).await {
            Ok(statuses) => Ok(json!({"timed_out": false, "sessions": statuses?})),
            Err(_) => {
                let statuses = parsed
                    .iter()
                    .map(|(id, _)| self.status(*id, caller_root))
                    .collect::<Result<Vec<_>>>()?;
                Ok(json!({"timed_out": true, "sessions": statuses}))
            }
        }
    }
}

impl SessionControl {
    pub(crate) fn definitions() -> Vec<Value> {
        vec![
            tool(
                "session_create",
                "Create a fresh persistent CodeCrab session with an isolated context and start its first turn asynchronously.",
                json!({
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string"},
                        "project": {"type": "string", "description": "Existing project directory; defaults to the caller's project"}
                    },
                    "required": ["prompt"]
                }),
            ),
            tool(
                "session_list",
                "List controllable persisted and live sessions. Defaults to the caller's project and excludes the caller.",
                json!({
                    "type": "object",
                    "properties": {
                        "project": {"type": "string"},
                        "all_projects": {"type": "boolean"},
                        "include_current": {"type": "boolean"}
                    }
                }),
            ),
            tool(
                "session_status",
                "Get cheap content-free lifecycle, transcript outline, goal, and current activity status for sessions.",
                json!({
                    "type": "object",
                    "properties": {
                        "session_ids": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 8,
                            "items": {"type": "string"}
                        }
                    },
                    "required": ["session_ids"]
                }),
            ),
            tool(
                "session_messages",
                "Read a bounded page of visible user and assistant messages from one session.",
                json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 100},
                        "cursor": {"type": "string"}
                    },
                    "required": ["session_id"]
                }),
            ),
            tool(
                "session_send",
                "Send a visible prompt to an idle session and return as soon as its turn is accepted.",
                json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"},
                        "prompt": {"type": "string"}
                    },
                    "required": ["session_id", "prompt"]
                }),
            ),
            tool(
                "session_stop",
                "Request cancellation of exactly one target session's active turn without deleting the session.",
                json!({
                    "type": "object",
                    "properties": {"session_id": {"type": "string"}},
                    "required": ["session_id"]
                }),
            ),
            tool(
                "session_wait",
                "Wait efficiently for any target session's observation revision to change. This tool is a response barrier.",
                json!({
                    "type": "object",
                    "properties": {
                        "targets": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 8,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "session_id": {"type": "string"},
                                    "after_revision": {"type": "integer", "minimum": 0}
                                },
                                "required": ["session_id", "after_revision"]
                            }
                        },
                        "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 60000}
                    },
                    "required": ["targets"]
                }),
            ),
        ]
    }

    pub(crate) async fn execute(&self, name: &str, args: &Value) -> Result<Value> {
        let inner = self
            .coordinator
            .upgrade()
            .context("session coordinator is no longer available")?;
        let coordinator = SessionCoordinator { inner };
        match name {
            "session_create" => {
                coordinator
                    .create_child(self.caller_session_id, &self.caller_project_root, args)
                    .await
            }
            "session_list" => {
                coordinator.list_sessions(self.caller_session_id, &self.caller_project_root, args)
            }
            "session_status" => coordinator.statuses(&self.caller_project_root, args),
            "session_messages" => coordinator.messages(&self.caller_project_root, args),
            "session_send" => {
                coordinator.send(self.caller_session_id, &self.caller_project_root, args)
            }
            "session_stop" => {
                coordinator.stop(self.caller_session_id, &self.caller_project_root, args)
            }
            "session_wait" => {
                coordinator
                    .wait(self.caller_session_id, &self.caller_project_root, args)
                    .await
            }
            _ => anyhow::bail!("unknown session control tool {name:?}"),
        }
    }
}

fn status_value(
    root: &Path,
    session: &Session,
    observation: Option<&ConversationObservation>,
    live: bool,
) -> Value {
    let persisted = persisted_observation(session);
    let observation = observation.unwrap_or(&persisted);
    json!({
        "session_id": session.id,
        "project_root": root,
        "title": observation.title,
        "parent_session_id": session.parent_session_id,
        "live": live,
        "lifecycle": observation.lifecycle,
        "observation_revision": observation.revision,
        "active_turn_started_at": observation.active_turn_started_at,
        "latest_observed_event_at": observation.latest_event_at,
        "catalog_error": observation.catalog_error,
        "last_turn": observation.last_turn,
        "visible_goal": observation.visible_goal,
        "messages": observation.messages.iter().map(|message| json!({
            "id": message.id,
            "role": message.role,
            "created_at": message.created_at,
            "partial": message.partial,
        })).collect::<Vec<_>>(),
        "activity": observation.activity,
    })
}

fn persisted_observation(session: &Session) -> ConversationObservation {
    let mut messages = session
        .messages
        .active_entries()
        .filter(|(_, message)| {
            !message.hidden && matches!(message.role, Role::User | Role::Assistant)
        })
        .map(|(_, message)| {
            let created_at = message.created_at.unwrap_or(session.updated_at);
            let role = if matches!(message.role, Role::User) {
                "user"
            } else {
                "assistant"
            };
            let id = message
                .sequence
                .map(|sequence| format!("assistant:{sequence}"))
                .unwrap_or_else(|| {
                    format!(
                        "{role}:{}",
                        created_at.to_rfc3339_opts(SecondsFormat::Nanos, true)
                    )
                });
            crate::conversation::ObservedMessage {
                id,
                role,
                created_at,
                content: message.content.clone().unwrap_or_default(),
                partial: false,
            }
        })
        .collect::<Vec<_>>();
    let last_turn = session.turns.last().and_then(|turn| {
        Some(crate::conversation::ObservedTurn {
            outcome: turn.outcome.unwrap_or(TurnOutcome::Completed),
            started_at: turn.started_at,
            ended_at: turn.completed_at?,
        })
    });
    if last_turn
        .as_ref()
        .is_some_and(|turn| turn.outcome != TurnOutcome::Completed)
        && let Some(message) = messages
            .iter_mut()
            .rev()
            .find(|message| message.role == "assistant")
    {
        message.partial = true;
    }
    let visible_goal = session
        .visible_goal_id
        .and_then(|id| session.goals.iter().find(|goal| goal.id == id))
        .map(|goal| crate::conversation::ObservedGoal {
            id: goal.id,
            status: goal.status,
        });
    let activity =
        session
            .activities
            .last()
            .map(|activity| crate::conversation::ObservedActivity {
                tool: activity.tool.clone(),
                status: activity.status,
                started_at: activity.started_at,
                completed_at: activity.completed_at,
            });
    ConversationObservation {
        revision: 0,
        title: session.title.clone(),
        lifecycle: ConversationLifecycle::Idle,
        active_turn_started_at: None,
        latest_event_at: session.updated_at,
        catalog_error: None,
        last_turn,
        visible_goal,
        messages,
        activity,
    }
}

fn required_nonempty<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    let value = args
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument {name:?}"))?
        .trim();
    if value.is_empty() {
        anyhow::bail!("{name} cannot be empty");
    }
    Ok(value)
}

fn required_id(args: &Value, name: &str) -> Result<Uuid> {
    let value = args
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument {name:?}"))?;
    Uuid::parse_str(value).with_context(|| format!("{name} must be a full UUID"))
}

fn required_ids(args: &Value, name: &str) -> Result<Vec<Uuid>> {
    let values = args
        .get(name)
        .and_then(Value::as_array)
        .with_context(|| format!("missing array argument {name:?}"))?;
    if values.is_empty() || values.len() > 8 {
        anyhow::bail!("{name} must contain between 1 and 8 full UUIDs");
    }
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .context("session ID must be a string")
                .and_then(|value| Uuid::parse_str(value).context("session ID must be a full UUID"))
        })
        .collect()
}

fn reject_self(caller: Uuid, target: Uuid, operation: &str) -> Result<()> {
    if caller == target {
        anyhow::bail!("{operation} cannot target the calling session");
    }
    Ok(())
}

fn parse_cursor(cursor: &str) -> Result<(u64, usize)> {
    let mut parts = cursor.split(':');
    if parts.next() != Some("v1") {
        anyhow::bail!("invalid message cursor");
    }
    let revision = parts
        .next()
        .context("invalid message cursor")?
        .parse()
        .context("invalid message cursor")?;
    let offset = parts
        .next()
        .context("invalid message cursor")?
        .parse()
        .context("invalid message cursor")?;
    if parts.next().is_some() {
        anyhow::bail!("invalid message cursor");
    }
    Ok((revision, offset))
}

fn tool(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters
        }
    })
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Notify,
    };

    use super::*;

    fn test_coordinator(root: &Path, mut config: Config) -> (SessionCoordinator, SessionRegistry) {
        let provider = config.providers.get_mut(&config.active_provider).unwrap();
        provider.fetch_models = false;
        provider
            .model_capabilities
            .entry(provider.model.clone())
            .or_default();
        let registry = SessionRegistry::at(root.join("global-config.toml"));
        let coordinator = SessionCoordinator::new(
            config,
            registry.clone(),
            DebugOutput::default(),
            DiagnosticLog::stderr(),
            root.join(".test-global-config").join("AGENTS.md"),
        );
        (coordinator, registry)
    }

    #[test]
    fn exposes_distinct_session_control_contracts() {
        let definitions = SessionControl::definitions();
        let names = definitions
            .iter()
            .map(|definition| definition["function"]["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "session_create",
                "session_list",
                "session_status",
                "session_messages",
                "session_send",
                "session_stop",
                "session_wait",
            ]
        );
    }

    #[tokio::test]
    async fn child_is_isolated_persisted_observable_cancellable_and_reusable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let first_delta = Arc::new(Notify::new());
        let first_delta_server = first_delta.clone();
        let second_completed = Arc::new(Notify::new());
        let second_completed_server = second_completed.clone();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            crate::test_support::read_http_request(&mut first).await;
            first
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let event = "data: {\"choices\":[{\"delta\":{\"content\":\"partial child\"}}]}\n\n";
            first
                .write_all(format!("{:X}\r\n{event}\r\n", event.len()).as_bytes())
                .await
                .unwrap();
            first.flush().await.unwrap();
            first_delta_server.notify_one();
            let mut buffer = [0; 32];
            while first.read(&mut buffer).await.unwrap_or(0) != 0 {}

            let (mut second, _) = listener.accept().await.unwrap();
            crate::test_support::read_http_request(&mut second).await;
            second
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let event = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"reused\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            second
                .write_all(format!("{:X}\r\n{event}\r\n", event.len()).as_bytes())
                .await
                .unwrap();
            second.write_all(b"0\r\n\r\n").await.unwrap();
            second_completed_server.notify_one();
        });

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = Config::test("mock-model", format!("http://{address}/v1"));
        let (coordinator, registry) = test_coordinator(&root, config);
        let caller = Uuid::new_v4();
        let created = coordinator
            .create_child(caller, &root, &json!({"prompt": "isolated task"}))
            .await
            .unwrap();
        let child = Uuid::parse_str(created["session_id"].as_str().unwrap()).unwrap();

        if tokio::time::timeout(Duration::from_secs(2), first_delta.notified())
            .await
            .is_err()
        {
            panic!(
                "child did not stream: {}",
                coordinator.status(child, &root).unwrap()
            );
        }
        for _ in 0..100 {
            if coordinator
                .inner
                .manager
                .get(child)
                .unwrap()
                .observation()
                .messages
                .iter()
                .any(|message| message.partial)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let status = coordinator.status(child, &root).unwrap();
        assert_eq!(status["parent_session_id"], caller.to_string());
        assert_eq!(status["lifecycle"], "running");
        assert_eq!(status["messages"][0]["role"], "user");
        assert_eq!(status["messages"][1]["partial"], true);
        assert_eq!(status["activity"], Value::Null);

        let after_revision = status["observation_revision"].as_u64().unwrap();
        let waiting_coordinator = coordinator.clone();
        let waiting_root = root.clone();
        let waiter = tokio::spawn(async move {
            waiting_coordinator
                .wait(
                    caller,
                    &waiting_root,
                    &json!({
                        "targets": [{
                            "session_id": child,
                            "after_revision": after_revision
                        }],
                        "timeout_ms": 2_000
                    }),
                )
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let stopped = coordinator
            .stop(caller, &root, &json!({"session_id": child}))
            .unwrap();
        assert_eq!(stopped["cancellation_requested"], true);
        assert_eq!(waiter.await.unwrap()["timed_out"], false);
        let handle = coordinator.inner.manager.get(child).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while handle.is_running() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(handle.lifecycle(), ConversationLifecycle::Stopped);
        let persisted = SessionStore::new(&root)
            .unwrap()
            .load(Some(&child.to_string()))
            .unwrap();
        assert_eq!(persisted.parent_session_id, Some(caller));
        assert_eq!(
            persisted.turns.last().unwrap().outcome,
            Some(TurnOutcome::Cancelled)
        );
        assert_eq!(persisted.messages.len(), 2);
        assert!(
            registry
                .directories()
                .unwrap()
                .iter()
                .any(|registered| paths_equal(registered, &root))
        );

        coordinator
            .send(
                caller,
                &root,
                &json!({"session_id": child, "prompt": "continue independently"}),
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), second_completed.notified())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while handle.is_running() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let messages = coordinator
            .messages(&root, &json!({"session_id": child, "limit": 100}))
            .unwrap();
        assert_eq!(messages["messages"].as_array().unwrap().len(), 4);
        assert_eq!(messages["messages"][3]["content"].as_str(), Some("reused"));
        assert_eq!(messages["messages"][3]["partial"], false);
        assert!(
            coordinator
                .send(caller, &root, &json!({"session_id": caller, "prompt": "x"}))
                .unwrap_err()
                .to_string()
                .contains("calling session")
        );

        coordinator.manager().shutdown_all().await.unwrap();
        server.await.unwrap();
    }
}
