use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result};
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

#[derive(Clone)]
pub(crate) struct ConversationHandle {
    commands: mpsc::UnboundedSender<ConversationCommand>,
    snapshot: watch::Receiver<ConversationSnapshot>,
    active_cancellation: Arc<Mutex<Option<watch::Sender<bool>>>>,
    command_gate: Arc<Mutex<()>>,
    accepting_commands: Arc<AtomicBool>,
    turn_reserved: Arc<AtomicBool>,
}

enum ConversationCommand {
    Turn {
        prompt: String,
        hidden: bool,
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
        spawn_worker(run_worker(
            agent,
            registry,
            command_rx,
            snapshot_tx,
            active_cancellation.clone(),
            turn_reserved.clone(),
        ))?;
        Ok(Self {
            commands,
            snapshot: snapshot_rx,
            active_cancellation,
            command_gate,
            accepting_commands,
            turn_reserved,
        })
    }

    pub(crate) fn snapshot(&self) -> ConversationSnapshot {
        self.snapshot.borrow().clone()
    }

    pub(crate) fn is_running(&self) -> bool {
        self.turn_reserved.load(Ordering::Acquire)
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
        self.start_turn_kind(prompt, false, events)
    }

    pub(crate) fn start_goal_continuation(
        &self,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_kind(String::new(), true, events)
    }

    fn start_turn_kind(
        &self,
        prompt: String,
        hidden: bool,
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
        let (reply, response) = oneshot::channel();
        if let Err(error) = self.send_unchecked(ConversationCommand::Turn {
            prompt,
            hidden,
            events,
            cancellation,
            reply,
        }) {
            *self
                .active_cancellation
                .lock()
                .expect("conversation cancellation mutex poisoned") = None;
            self.turn_reserved.store(false, Ordering::Release);
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
            let (reply, response) = oneshot::channel();
            if let Err(error) = self.send_unchecked(ConversationCommand::Shutdown { reply }) {
                self.accepting_commands.store(true, Ordering::Release);
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
) {
    while let Some(command) = commands.recv().await {
        let stop = match command {
            ConversationCommand::Turn {
                prompt,
                hidden,
                events,
                cancellation,
                reply,
            } => {
                let result = if hidden {
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
                true
            }
        };
        if stop {
            break;
        }
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
}
