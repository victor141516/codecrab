use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use uuid::Uuid;

use crate::{
    agent::Agent,
    config::SessionRegistry,
    events::AgentEvent,
    provider::ModelSelection,
    session::{Session, SessionStore},
};

#[derive(Clone)]
pub(crate) struct ConversationSnapshot {
    pub project_root: PathBuf,
    pub session: Session,
    pub skills: Vec<ConversationSkill>,
}

#[derive(Clone)]
pub(crate) struct ConversationSkill {
    pub name: String,
    pub description: String,
    pub scope: &'static str,
}

pub(crate) struct ConversationTurn {
    pub result: Result<String>,
    pub snapshot: ConversationSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConversationLifecycle {
    Idle,
    Running,
    Stopping,
    Stopped,
    Failed,
}

impl ConversationLifecycle {
    fn as_u8(self) -> u8 {
        self as u8
    }

    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Idle,
            1 => Self::Running,
            2 => Self::Stopping,
            3 => Self::Stopped,
            _ => Self::Failed,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ConversationHandle {
    commands: mpsc::UnboundedSender<ConversationCommand>,
    snapshot: watch::Receiver<ConversationSnapshot>,
    active_cancellation: Arc<Mutex<Option<watch::Sender<bool>>>>,
    command_gate: Arc<Mutex<()>>,
    accepting_commands: Arc<AtomicBool>,
    turn_reserved: Arc<AtomicBool>,
    lifecycle: Arc<AtomicU8>,
}

enum ConversationCommand {
    Turn {
        prompt: String,
        hidden: bool,
        edit_node_id: Option<Uuid>,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
        cancellation: watch::Receiver<bool>,
        reply: oneshot::Sender<ConversationTurn>,
    },
    SetModel {
        selection: ModelSelection,
        reply: oneshot::Sender<Result<ConversationSnapshot>>,
    },
    Clear {
        reply: oneshot::Sender<Result<ConversationSnapshot>>,
    },
    SelectBranch {
        node_id: Uuid,
        reply: oneshot::Sender<Result<ConversationSnapshot>>,
    },
    CreateGoal {
        objective: String,
        reply: oneshot::Sender<Result<ConversationSnapshot>>,
    },
    EditGoal {
        id: Uuid,
        objective: String,
        activate: bool,
        reply: oneshot::Sender<Result<Option<ConversationSnapshot>>>,
    },
    ActivateGoal {
        id: Uuid,
        reply: oneshot::Sender<Result<Option<ConversationSnapshot>>>,
    },
    PauseGoal {
        id: Uuid,
        reply: oneshot::Sender<Result<Option<ConversationSnapshot>>>,
    },
    DeleteGoal {
        id: Uuid,
        reply: oneshot::Sender<Result<Option<ConversationSnapshot>>>,
    },
    Persist {
        reply: oneshot::Sender<Result<ConversationSnapshot>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<ConversationSnapshot>>,
    },
}

impl ConversationHandle {
    pub(crate) fn spawn(agent: Agent, registry: SessionRegistry) -> Result<Self> {
        let initial = snapshot(&agent);
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (snapshot_tx, snapshot_rx) = watch::channel(initial);
        let active_cancellation = Arc::new(Mutex::new(None));
        let command_gate = Arc::new(Mutex::new(()));
        let accepting_commands = Arc::new(AtomicBool::new(true));
        let turn_reserved = Arc::new(AtomicBool::new(false));
        let lifecycle = Arc::new(AtomicU8::new(ConversationLifecycle::Idle.as_u8()));
        spawn_worker(run_worker(
            agent,
            registry,
            command_rx,
            snapshot_tx,
            active_cancellation.clone(),
            turn_reserved.clone(),
            lifecycle.clone(),
        ))?;
        Ok(Self {
            commands,
            snapshot: snapshot_rx,
            active_cancellation,
            command_gate,
            accepting_commands,
            turn_reserved,
            lifecycle,
        })
    }

    pub(crate) fn snapshot(&self) -> ConversationSnapshot {
        self.snapshot.borrow().clone()
    }

    pub(crate) fn is_running(&self) -> bool {
        self.turn_reserved.load(Ordering::Acquire)
    }

    pub(crate) fn lifecycle(&self) -> ConversationLifecycle {
        ConversationLifecycle::from_u8(self.lifecycle.load(Ordering::Acquire))
    }

    pub(crate) fn cancel(&self) -> bool {
        self.active_cancellation
            .lock()
            .expect("conversation cancellation mutex poisoned")
            .as_ref()
            .is_some_and(|sender| sender.send(true).is_ok())
    }

    pub(crate) fn start_turn(
        &self,
        prompt: String,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_kind(prompt, false, None, events)
    }

    pub(crate) fn start_edit_turn(
        &self,
        node_id: Uuid,
        prompt: String,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_kind(prompt, false, Some(node_id), events)
    }

    pub(crate) fn start_goal_continuation(
        &self,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_kind(String::new(), true, None, events)
    }

    fn start_turn_kind(
        &self,
        prompt: String,
        hidden: bool,
        edit_node_id: Option<Uuid>,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        let _gate = self
            .command_gate
            .lock()
            .expect("conversation command gate poisoned");
        self.ensure_accepting_commands()?;
        if self
            .turn_reserved
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            anyhow::bail!("a conversation turn is already running");
        }
        let (cancel_tx, cancellation) = watch::channel(false);
        *self
            .active_cancellation
            .lock()
            .expect("conversation cancellation mutex poisoned") = Some(cancel_tx);
        self.lifecycle
            .store(ConversationLifecycle::Running.as_u8(), Ordering::Release);
        let (reply, response) = oneshot::channel();
        if let Err(error) = self.send_unchecked(ConversationCommand::Turn {
            prompt,
            hidden,
            edit_node_id,
            events,
            cancellation,
            reply,
        }) {
            *self
                .active_cancellation
                .lock()
                .expect("conversation cancellation mutex poisoned") = None;
            self.turn_reserved.store(false, Ordering::Release);
            self.lifecycle
                .store(ConversationLifecycle::Idle.as_u8(), Ordering::Release);
            return Err(error);
        }
        Ok(tokio::spawn(async move {
            response
                .await
                .context("conversation worker stopped during the turn")
        }))
    }

    pub(crate) async fn turn(&self, prompt: String) -> Result<ConversationTurn> {
        self.start_turn(prompt, None)?
            .await
            .context("conversation turn task failed")?
    }

    pub(crate) async fn set_model(
        &self,
        selection: ModelSelection,
    ) -> Result<ConversationSnapshot> {
        let (reply, response) = oneshot::channel();
        self.send(ConversationCommand::SetModel { selection, reply })?;
        receive(response, "setting the conversation model").await?
    }

    pub(crate) async fn clear(&self) -> Result<ConversationSnapshot> {
        let (reply, response) = oneshot::channel();
        self.send(ConversationCommand::Clear { reply })?;
        receive(response, "clearing the conversation").await?
    }

    pub(crate) async fn select_branch(&self, node_id: Uuid) -> Result<ConversationSnapshot> {
        let response = {
            let _gate = self
                .command_gate
                .lock()
                .expect("conversation command gate poisoned");
            self.ensure_accepting_commands()?;
            if self.is_running() {
                anyhow::bail!("wait for the active turn before selecting a conversation branch");
            }
            let (reply, response) = oneshot::channel();
            self.send_unchecked(ConversationCommand::SelectBranch { node_id, reply })?;
            response
        };
        receive(response, "selecting the conversation branch").await?
    }

    pub(crate) async fn create_goal(&self, objective: String) -> Result<ConversationSnapshot> {
        let (reply, response) = oneshot::channel();
        self.send(ConversationCommand::CreateGoal { objective, reply })?;
        receive(response, "creating a goal").await?
    }

    pub(crate) async fn edit_goal(
        &self,
        id: Uuid,
        objective: String,
        activate: bool,
    ) -> Result<Option<ConversationSnapshot>> {
        let (reply, response) = oneshot::channel();
        self.send(ConversationCommand::EditGoal {
            id,
            objective,
            activate,
            reply,
        })?;
        receive(response, "editing a goal").await?
    }

    pub(crate) async fn activate_goal(&self, id: Uuid) -> Result<Option<ConversationSnapshot>> {
        let (reply, response) = oneshot::channel();
        self.send(ConversationCommand::ActivateGoal { id, reply })?;
        receive(response, "activating a goal").await?
    }

    pub(crate) async fn pause_goal(&self, id: Uuid) -> Result<Option<ConversationSnapshot>> {
        let (reply, response) = oneshot::channel();
        self.send(ConversationCommand::PauseGoal { id, reply })?;
        receive(response, "pausing a goal").await?
    }

    pub(crate) async fn delete_goal(&self, id: Uuid) -> Result<Option<ConversationSnapshot>> {
        let (reply, response) = oneshot::channel();
        self.send(ConversationCommand::DeleteGoal { id, reply })?;
        receive(response, "deleting a goal").await?
    }

    pub(crate) async fn persist(&self) -> Result<ConversationSnapshot> {
        self.request_snapshot_command(
            |reply| ConversationCommand::Persist { reply },
            "persisting the conversation",
        )
        .await
    }

    pub(crate) async fn persist_if_idle(&self) -> Result<ConversationSnapshot> {
        let response = {
            let _gate = self
                .command_gate
                .lock()
                .expect("conversation command gate poisoned");
            self.ensure_accepting_commands()?;
            if self.is_running() {
                return Ok(self.snapshot());
            }
            let (reply, response) = oneshot::channel();
            self.send_unchecked(ConversationCommand::Persist { reply })?;
            response
        };
        receive(response, "persisting the idle conversation").await?
    }

    pub(crate) async fn shutdown(&self) -> Result<ConversationSnapshot> {
        let response = {
            let _gate = self
                .command_gate
                .lock()
                .expect("conversation command gate poisoned");
            self.ensure_accepting_commands()?;
            if self.is_running() {
                anyhow::bail!("wait for the active turn before shutting down the conversation");
            }
            self.accepting_commands.store(false, Ordering::Release);
            self.lifecycle
                .store(ConversationLifecycle::Stopping.as_u8(), Ordering::Release);
            let (reply, response) = oneshot::channel();
            if let Err(error) = self.send_unchecked(ConversationCommand::Shutdown { reply }) {
                self.accepting_commands.store(true, Ordering::Release);
                self.lifecycle
                    .store(ConversationLifecycle::Idle.as_u8(), Ordering::Release);
                return Err(error);
            }
            response
        };
        receive(response, "shutting down the conversation").await?
    }

    async fn request_snapshot_command(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<ConversationSnapshot>>) -> ConversationCommand,
        action: &str,
    ) -> Result<ConversationSnapshot> {
        let response = {
            let _gate = self
                .command_gate
                .lock()
                .expect("conversation command gate poisoned");
            self.ensure_accepting_commands()?;
            let (reply, response) = oneshot::channel();
            self.send_unchecked(command(reply))?;
            response
        };
        receive(response, action).await?
    }

    fn ensure_accepting_commands(&self) -> Result<()> {
        if !self.accepting_commands.load(Ordering::Acquire) {
            anyhow::bail!("conversation worker is shutting down");
        }
        Ok(())
    }

    fn send(&self, command: ConversationCommand) -> Result<()> {
        let _gate = self
            .command_gate
            .lock()
            .expect("conversation command gate poisoned");
        self.ensure_accepting_commands()?;
        self.send_unchecked(command)
    }

    fn send_unchecked(&self, command: ConversationCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow::anyhow!("conversation worker is no longer running"))
    }
}

async fn receive<T>(receiver: oneshot::Receiver<T>, action: &str) -> Result<T> {
    receiver
        .await
        .with_context(|| format!("conversation worker stopped while {action}"))
}

async fn run_worker(
    mut agent: Agent,
    registry: SessionRegistry,
    mut commands: mpsc::UnboundedReceiver<ConversationCommand>,
    snapshots: watch::Sender<ConversationSnapshot>,
    active_cancellation: Arc<Mutex<Option<watch::Sender<bool>>>>,
    turn_reserved: Arc<AtomicBool>,
    lifecycle: Arc<AtomicU8>,
) {
    let mut explicitly_stopped = false;
    while let Some(command) = commands.recv().await {
        let stop = match command {
            ConversationCommand::Turn {
                prompt,
                hidden,
                edit_node_id,
                events,
                cancellation,
                reply,
            } => {
                let result = if let Some(node_id) = edit_node_id {
                    agent
                        .edit_turn_controlled(node_id, &prompt, events, cancellation)
                        .await
                } else if hidden {
                    match events {
                        Some(events) => agent.continue_goal_with_events(events, cancellation).await,
                        None => agent.continue_goal_controlled(None, cancellation).await,
                    }
                } else {
                    agent.turn_controlled(&prompt, events, cancellation).await
                };
                *active_cancellation
                    .lock()
                    .expect("conversation cancellation mutex poisoned") = None;
                let save_result = persist(&agent, &registry);
                let current = snapshot(&agent);
                let _ = snapshots.send(current.clone());
                let result = match (result, save_result) {
                    (Ok(answer), Ok(())) => Ok(answer),
                    (Ok(_), Err(save_error)) => Err(save_error),
                    (Err(turn_error), Ok(())) => Err(turn_error),
                    (Err(turn_error), Err(save_error)) => Err(turn_error.context(format!(
                        "the conversation also could not be persisted: {save_error:#}"
                    ))),
                };
                turn_reserved.store(false, Ordering::Release);
                lifecycle.store(ConversationLifecycle::Idle.as_u8(), Ordering::Release);
                let _ = reply.send(ConversationTurn {
                    result,
                    snapshot: current,
                });
                false
            }
            ConversationCommand::SetModel { selection, reply } => {
                agent.set_model_selection(selection);
                reply_snapshot(&agent, &registry, &snapshots, reply);
                false
            }
            ConversationCommand::Clear { reply } => {
                agent.clear();
                reply_snapshot(&agent, &registry, &snapshots, reply);
                false
            }
            ConversationCommand::SelectBranch { node_id, reply } => {
                let result = agent
                    .select_branch(node_id)
                    .and_then(|_| persist(&agent, &registry).map(|_| snapshot(&agent)));
                if let Ok(current) = &result {
                    let _ = snapshots.send(current.clone());
                }
                let _ = reply.send(result);
                false
            }
            ConversationCommand::CreateGoal { objective, reply } => {
                agent.create_goal(objective);
                reply_snapshot(&agent, &registry, &snapshots, reply);
                false
            }
            ConversationCommand::EditGoal {
                id,
                objective,
                activate,
                reply,
            } => {
                let changed = agent.edit_goal(id, objective);
                if changed && activate {
                    agent.activate_goal(id);
                }
                reply_optional_snapshot(changed, &agent, &registry, &snapshots, reply);
                false
            }
            ConversationCommand::ActivateGoal { id, reply } => {
                let changed = agent.activate_goal(id);
                reply_optional_snapshot(changed, &agent, &registry, &snapshots, reply);
                false
            }
            ConversationCommand::PauseGoal { id, reply } => {
                let changed = agent.pause_goal(id);
                reply_optional_snapshot(changed, &agent, &registry, &snapshots, reply);
                false
            }
            ConversationCommand::DeleteGoal { id, reply } => {
                let changed = agent.delete_goal(id);
                reply_optional_snapshot(changed, &agent, &registry, &snapshots, reply);
                false
            }
            ConversationCommand::Persist { reply } => {
                reply_snapshot(&agent, &registry, &snapshots, reply);
                false
            }
            ConversationCommand::Shutdown { reply } => {
                reply_snapshot(&agent, &registry, &snapshots, reply);
                explicitly_stopped = true;
                true
            }
        };
        if stop {
            break;
        }
    }
    lifecycle.store(
        if explicitly_stopped {
            ConversationLifecycle::Stopped.as_u8()
        } else {
            ConversationLifecycle::Failed.as_u8()
        },
        Ordering::Release,
    );
}

#[derive(Clone, Serialize)]
pub(crate) struct ConversationStatus {
    pub id: Uuid,
    pub project_root: PathBuf,
    pub lifecycle: ConversationLifecycle,
}

#[derive(Clone)]
pub(crate) struct ConversationManager {
    conversations: Arc<RwLock<HashMap<Uuid, ConversationHandle>>>,
    registry: SessionRegistry,
}

impl ConversationManager {
    pub(crate) fn new(registry: SessionRegistry) -> Self {
        Self {
            conversations: Arc::new(RwLock::new(HashMap::new())),
            registry,
        }
    }

    pub(crate) fn with_handle(registry: SessionRegistry, handle: ConversationHandle) -> Self {
        let manager = Self::new(registry);
        let id = handle.snapshot().session.id;
        manager
            .conversations
            .write()
            .expect("conversation manager lock poisoned")
            .insert(id, handle);
        manager
    }

    pub(crate) fn install(&self, agent: Agent) -> Result<ConversationHandle> {
        let id = agent.session().id;
        let mut conversations = self
            .conversations
            .write()
            .expect("conversation manager lock poisoned");
        if let Some(existing) = conversations.get(&id) {
            return Ok(existing.clone());
        }
        let handle = ConversationHandle::spawn(agent, self.registry.clone())?;
        conversations.insert(id, handle.clone());
        Ok(handle)
    }

    pub(crate) fn get(&self, id: Uuid) -> Option<ConversationHandle> {
        self.conversations
            .read()
            .expect("conversation manager lock poisoned")
            .get(&id)
            .cloned()
    }

    pub(crate) fn statuses(&self) -> Vec<ConversationStatus> {
        self.conversations
            .read()
            .expect("conversation manager lock poisoned")
            .iter()
            .map(|(id, handle)| {
                let snapshot = handle.snapshot();
                ConversationStatus {
                    id: *id,
                    project_root: snapshot.project_root,
                    lifecycle: handle.lifecycle(),
                }
            })
            .collect()
    }

    pub(crate) fn take_if_idle(&self, id: Uuid) -> Result<Option<ConversationHandle>> {
        let mut conversations = self
            .conversations
            .write()
            .expect("conversation manager lock poisoned");
        let Some(handle) = conversations.get(&id) else {
            return Ok(None);
        };
        if handle.is_running() {
            anyhow::bail!("wait for the active turn before deleting its session");
        }
        Ok(conversations.remove(&id))
    }

    pub(crate) fn cancel(&self, id: Uuid) -> bool {
        self.get(id).is_some_and(|handle| handle.cancel())
    }

    pub(crate) fn cancel_all(&self) {
        for handle in self
            .conversations
            .read()
            .expect("conversation manager lock poisoned")
            .values()
        {
            handle.cancel();
        }
    }

    pub(crate) async fn shutdown_all(&self) -> Result<Vec<ConversationSnapshot>> {
        let handles = {
            let mut conversations = self
                .conversations
                .write()
                .expect("conversation manager lock poisoned");
            conversations
                .drain()
                .map(|(_, handle)| handle)
                .collect::<Vec<_>>()
        };
        let mut snapshots = Vec::with_capacity(handles.len());
        let mut first_error = None;
        for handle in handles {
            match handle.shutdown().await {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(snapshots)
    }
}

fn reply_snapshot(
    agent: &Agent,
    registry: &SessionRegistry,
    snapshots: &watch::Sender<ConversationSnapshot>,
    reply: oneshot::Sender<Result<ConversationSnapshot>>,
) {
    let current = snapshot(agent);
    let _ = snapshots.send(current.clone());
    let result = persist(agent, registry).map(|()| current);
    let _ = reply.send(result);
}

fn reply_optional_snapshot(
    changed: bool,
    agent: &Agent,
    registry: &SessionRegistry,
    snapshots: &watch::Sender<ConversationSnapshot>,
    reply: oneshot::Sender<Result<Option<ConversationSnapshot>>>,
) {
    let result = if changed {
        let current = snapshot(agent);
        let _ = snapshots.send(current.clone());
        persist(agent, registry).map(|()| Some(current))
    } else {
        Ok(None)
    };
    let _ = reply.send(result);
}

fn snapshot(agent: &Agent) -> ConversationSnapshot {
    ConversationSnapshot {
        project_root: agent.project_root().to_path_buf(),
        session: agent.session().clone(),
        skills: agent
            .skills()
            .iter()
            .map(|skill| ConversationSkill {
                name: skill.name.clone(),
                description: skill.description.clone(),
                scope: skill.scope.label(),
            })
            .collect(),
    }
}

fn persist(agent: &Agent, registry: &SessionRegistry) -> Result<()> {
    let root = agent.project_root();
    SessionStore::new(root)?.save(agent.session())?;
    registry.register(root)
}

fn spawn_worker(future: impl Future<Output = ()> + Send + 'static) -> Result<()> {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(future);
        return Ok(());
    }
    #[cfg(test)]
    {
        test_runtime().spawn(future);
        Ok(())
    }
    #[cfg(not(test))]
    anyhow::bail!("conversation workers require a Tokio runtime")
}

#[cfg(test)]
fn test_runtime() -> &'static tokio::runtime::Runtime {
    use std::sync::OnceLock;

    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test conversation runtime")
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        config::Config, provider::OpenAiCompatible, skills::SkillRegistry, tools::ToolBox,
    };

    fn test_handle(root: &Path) -> ConversationHandle {
        let config = Config::test("model", "http://127.0.0.1:1/v1");
        let provider = OpenAiCompatible::new(&config, &config.active_provider).unwrap();
        let session = SessionStore::new(root)
            .unwrap()
            .create_for_provider(config.active_provider.clone(), "model".into())
            .unwrap();
        let agent = Agent::new(
            provider,
            ToolBox::new(root.to_path_buf()),
            SkillRegistry::default(),
            session,
        )
        .unwrap();
        ConversationHandle::spawn(
            agent,
            SessionRegistry::at(root.join("test-global-config.toml")),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn worker_owns_and_persists_mutations_across_commands() {
        let root = tempfile::tempdir().unwrap();
        let handle = test_handle(root.path());
        let original = handle.snapshot().session.id;

        let snapshot = handle.create_goal("Ship it".into()).await.unwrap();
        assert_eq!(snapshot.session.id, original);
        assert_eq!(snapshot.session.goals[0].objective, "Ship it");

        let saved = SessionStore::new(root.path())
            .unwrap()
            .load(Some(&original.to_string()))
            .unwrap();
        assert_eq!(saved.goals[0].objective, "Ship it");
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_invalidates_every_existing_handle_clone() {
        let root = tempfile::tempdir().unwrap();
        let handle = test_handle(root.path());
        let stale = handle.clone();

        handle.shutdown().await.unwrap();

        let turn_error = match stale.start_turn("Too late".into(), None) {
            Ok(_) => panic!("a stale handle must not start a turn"),
            Err(error) => error,
        };
        assert!(format!("{turn_error:#}").contains("shutting down"));

        let mutation_error = match stale.create_goal("Too late".into()).await {
            Ok(_) => panic!("a stale handle must not mutate the conversation"),
            Err(error) => error,
        };
        assert!(format!("{mutation_error:#}").contains("shutting down"));
    }

    #[tokio::test]
    async fn rejected_shutdown_keeps_the_worker_available() {
        let root = tempfile::tempdir().unwrap();
        let handle = test_handle(root.path());
        handle.turn_reserved.store(true, Ordering::Release);

        let error = match handle.shutdown().await {
            Ok(_) => panic!("an active conversation must reject shutdown"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("active turn"));

        handle.turn_reserved.store(false, Ordering::Release);
        let snapshot = handle.create_goal("Still alive".into()).await.unwrap();
        assert_eq!(snapshot.session.goals[0].objective, "Still alive");
        handle.shutdown().await.unwrap();
    }

    #[test]
    fn a_second_turn_is_rejected_before_the_worker_dequeues_the_first() {
        let root = tempfile::tempdir().unwrap();
        let handle = test_handle(root.path());
        handle.turn_reserved.store(true, Ordering::Release);

        let error = match handle.start_turn("Second".into(), None) {
            Ok(_) => panic!("a reserved conversation must reject a second turn"),
            Err(error) => error,
        };

        assert!(format!("{error:#}").contains("already running"));
    }

    #[test]
    fn cancellation_is_out_of_band_from_the_worker_command_queue() {
        let root = tempfile::tempdir().unwrap();
        let handle = test_handle(root.path());
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        *handle.active_cancellation.lock().unwrap() = Some(cancel_tx);

        assert!(handle.cancel());
        assert!(*cancel_rx.borrow_and_update());
    }

    #[tokio::test]
    async fn manager_routes_cancellation_to_exactly_one_session() {
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let first = test_handle(first_root.path());
        let second = test_handle(second_root.path());
        let first_id = first.snapshot().session.id;
        let second_id = second.snapshot().session.id;
        let registry = SessionRegistry::at(first_root.path().join("manager-global-config.toml"));
        let manager = ConversationManager::with_handle(registry, first.clone());
        manager
            .conversations
            .write()
            .unwrap()
            .insert(second_id, second.clone());
        let (first_cancel, mut first_rx) = watch::channel(false);
        let (second_cancel, mut second_rx) = watch::channel(false);
        *first.active_cancellation.lock().unwrap() = Some(first_cancel);
        *second.active_cancellation.lock().unwrap() = Some(second_cancel);

        assert!(manager.cancel(first_id));
        assert!(*first_rx.borrow_and_update());
        assert!(!*second_rx.borrow_and_update());
        assert_eq!(manager.statuses().len(), 2);

        *first.active_cancellation.lock().unwrap() = None;
        *second.active_cancellation.lock().unwrap() = None;
        manager.shutdown_all().await.unwrap();
    }
}
