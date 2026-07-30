use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use uuid::Uuid;

use crate::{
    agent::{Agent, turn_was_cancelled},
    config::SessionRegistry,
    events::{ActivityStatus, AgentActivity, AgentEvent},
    provider::{Message, ModelCatalogEntry, ModelSelection, Role},
    session::{GoalStatus, Session, SessionStore, TurnOutcome},
};

#[derive(Clone)]
pub(crate) struct ConversationSnapshot {
    pub project_root: PathBuf,
    pub session: Session,
    pub skills: Vec<ConversationSkill>,
    pub model_catalog: Vec<ModelCatalogEntry>,
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

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ObservedMessage {
    pub id: String,
    pub role: &'static str,
    pub created_at: DateTime<Utc>,
    pub content: String,
    pub partial: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ObservedActivity {
    pub tool: String,
    pub status: ActivityStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ObservedTurn {
    pub outcome: TurnOutcome,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ObservedGoal {
    pub id: Uuid,
    pub status: GoalStatus,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConversationObservation {
    pub revision: u64,
    pub title: String,
    pub lifecycle: ConversationLifecycle,
    pub active_turn_started_at: Option<DateTime<Utc>>,
    pub latest_event_at: DateTime<Utc>,
    pub catalog_error: Option<String>,
    pub last_turn: Option<ObservedTurn>,
    pub visible_goal: Option<ObservedGoal>,
    pub messages: Vec<ObservedMessage>,
    pub activity: Option<ObservedActivity>,
}

#[derive(Clone)]
pub(crate) struct ObservationHub {
    revision: Arc<AtomicU64>,
    changes: watch::Sender<u64>,
}

impl Default for ObservationHub {
    fn default() -> Self {
        let (changes, _) = watch::channel(0);
        Self {
            revision: Arc::new(AtomicU64::new(0)),
            changes,
        }
    }
}

impl ObservationHub {
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn publish(&self) {
        let revision = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.changes.send(revision);
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
    observation: Arc<Mutex<ConversationObservation>>,
    observation_hub: ObservationHub,
}

enum ConversationCommand {
    Turn {
        prompt: String,
        hidden: bool,
        edit_node_id: Option<Uuid>,
        initialize_catalog: bool,
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

struct ConversationWorkerState {
    registry: SessionRegistry,
    snapshots: watch::Sender<ConversationSnapshot>,
    active_cancellation: Arc<Mutex<Option<watch::Sender<bool>>>>,
    turn_reserved: Arc<AtomicBool>,
    lifecycle: Arc<AtomicU8>,
    observation: Arc<Mutex<ConversationObservation>>,
    observation_hub: ObservationHub,
}

impl ConversationHandle {
    #[cfg(test)]
    pub(crate) fn spawn(agent: Agent, registry: SessionRegistry) -> Result<Self> {
        Self::spawn_with_hub(agent, registry, ObservationHub::default())
    }

    pub(crate) fn spawn_with_hub(
        agent: Agent,
        registry: SessionRegistry,
        observation_hub: ObservationHub,
    ) -> Result<Self> {
        let initial = snapshot(&agent);
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (snapshot_tx, snapshot_rx) = watch::channel(initial.clone());
        let active_cancellation = Arc::new(Mutex::new(None));
        let command_gate = Arc::new(Mutex::new(()));
        let accepting_commands = Arc::new(AtomicBool::new(true));
        let turn_reserved = Arc::new(AtomicBool::new(false));
        let lifecycle = Arc::new(AtomicU8::new(ConversationLifecycle::Idle.as_u8()));
        let observation = Arc::new(Mutex::new(observation_from_snapshot(
            &initial,
            ConversationLifecycle::Idle,
            0,
        )));
        spawn_worker(run_worker(
            agent,
            command_rx,
            ConversationWorkerState {
                registry,
                snapshots: snapshot_tx,
                active_cancellation: active_cancellation.clone(),
                turn_reserved: turn_reserved.clone(),
                lifecycle: lifecycle.clone(),
                observation: observation.clone(),
                observation_hub: observation_hub.clone(),
            },
        ))?;
        Ok(Self {
            commands,
            snapshot: snapshot_rx,
            active_cancellation,
            command_gate,
            accepting_commands,
            turn_reserved,
            lifecycle,
            observation,
            observation_hub,
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

    pub(crate) fn observation(&self) -> ConversationObservation {
        self.observation
            .lock()
            .expect("conversation observation mutex poisoned")
            .clone()
    }

    pub(crate) fn cancel(&self) -> bool {
        let requested = self
            .active_cancellation
            .lock()
            .expect("conversation cancellation mutex poisoned")
            .as_ref()
            .is_some_and(|sender| sender.send(true).is_ok());
        if requested {
            self.lifecycle
                .store(ConversationLifecycle::Stopping.as_u8(), Ordering::Release);
            update_observation(&self.observation, &self.observation_hub, |observation| {
                observation.lifecycle = ConversationLifecycle::Stopping;
            });
        }
        requested
    }

    pub(crate) fn start_turn(
        &self,
        prompt: String,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_kind(prompt, false, None, false, events)
    }

    pub(crate) fn start_new_session_turn(
        &self,
        prompt: String,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_kind(prompt, false, None, true, None)
    }

    pub(crate) fn start_edit_turn(
        &self,
        node_id: Uuid,
        prompt: String,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_kind(prompt, false, Some(node_id), false, events)
    }

    pub(crate) fn start_goal_continuation(
        &self,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_kind(String::new(), true, None, false, events)
    }

    fn start_turn_kind(
        &self,
        prompt: String,
        hidden: bool,
        edit_node_id: Option<Uuid>,
        initialize_catalog: bool,
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
        update_observation(&self.observation, &self.observation_hub, |observation| {
            observation.lifecycle = ConversationLifecycle::Running;
            observation.active_turn_started_at = Some(Utc::now());
            observation.activity = None;
        });
        let (reply, response) = oneshot::channel();
        if let Err(error) = self.send_unchecked(ConversationCommand::Turn {
            prompt,
            hidden,
            edit_node_id,
            initialize_catalog,
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
            update_observation(&self.observation, &self.observation_hub, |observation| {
                observation.lifecycle = ConversationLifecycle::Idle;
                observation.active_turn_started_at = None;
            });
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
    mut commands: mpsc::UnboundedReceiver<ConversationCommand>,
    state: ConversationWorkerState,
) {
    let ConversationWorkerState {
        registry,
        snapshots,
        active_cancellation,
        turn_reserved,
        lifecycle,
        observation,
        observation_hub,
    } = state;
    let mut explicitly_stopped = false;
    while let Some(command) = commands.recv().await {
        let stop = match command {
            ConversationCommand::Turn {
                prompt,
                hidden,
                edit_node_id,
                initialize_catalog,
                events,
                mut cancellation,
                reply,
            } => {
                update_observation(&observation, &observation_hub, |_| {});
                let (internal_events, mut event_rx) = mpsc::unbounded_channel();
                let live_observation = observation.clone();
                let live_hub = observation_hub.clone();
                let forwarded_events = events.clone();
                let observer = tokio::spawn(async move {
                    while let Some(event) = event_rx.recv().await {
                        apply_agent_event(&live_observation, &live_hub, &event);
                        if let Some(events) = &forwarded_events {
                            let _ = events.send(event);
                        } else if let AgentEvent::Activity(activity) = &event
                            && activity.status == ActivityStatus::Running
                        {
                            eprintln!(
                                "\x1b[2m  crab → {} · {}\x1b[0m",
                                activity.title, activity.detail
                            );
                        }
                    }
                });
                if initialize_catalog {
                    let catalog_result = tokio::select! {
                        result = agent.fetch_models() => result,
                        () = wait_for_cancellation(&mut cancellation) => {
                            Err(crate::agent::TurnCancelled.into())
                        }
                    };
                    match catalog_result {
                        Ok(catalog) => {
                            agent.resolve_new_session_model(&catalog);
                        }
                        Err(error) if turn_was_cancelled(&error) => {}
                        Err(error) => {
                            let error = format!("{error:#}");
                            update_observation(&observation, &observation_hub, |observation| {
                                observation.catalog_error = Some(error)
                            });
                        }
                    }
                }
                let result = if let Some(node_id) = edit_node_id {
                    agent
                        .edit_turn_controlled(node_id, &prompt, Some(internal_events), cancellation)
                        .await
                } else if hidden {
                    agent
                        .continue_goal_with_events(internal_events, cancellation)
                        .await
                } else {
                    agent
                        .turn_controlled(&prompt, Some(internal_events), cancellation)
                        .await
                };
                let _ = observer.await;
                *active_cancellation
                    .lock()
                    .expect("conversation cancellation mutex poisoned") = None;
                let outcome = match &result {
                    Ok(_) => TurnOutcome::Completed,
                    Err(error) if turn_was_cancelled(error) => TurnOutcome::Cancelled,
                    Err(_) => TurnOutcome::Failed,
                };
                agent.record_turn_outcome(outcome);
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
                let terminal_lifecycle = match outcome {
                    TurnOutcome::Completed => ConversationLifecycle::Idle,
                    TurnOutcome::Cancelled => ConversationLifecycle::Stopped,
                    TurnOutcome::Failed => ConversationLifecycle::Failed,
                };
                lifecycle.store(terminal_lifecycle.as_u8(), Ordering::Release);
                reconcile_observation(&observation, &observation_hub, &current, terminal_lifecycle);
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
                let result = agent
                    .clear()
                    .and_then(|()| persist(&agent, &registry).map(|()| snapshot(&agent)));
                if let Ok(current) = &result {
                    let _ = snapshots.send(current.clone());
                }
                let _ = reply.send(result);
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
                let result = agent
                    .shutdown()
                    .and_then(|()| persist(&agent, &registry).map(|()| snapshot(&agent)));
                if let Ok(current) = &result {
                    let _ = snapshots.send(current.clone());
                }
                let _ = reply.send(result);
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
    update_observation(&observation, &observation_hub, |observation| {
        observation.lifecycle = if explicitly_stopped {
            ConversationLifecycle::Stopped
        } else {
            ConversationLifecycle::Failed
        };
        observation.active_turn_started_at = None;
    });
}

fn update_observation(
    observation: &Arc<Mutex<ConversationObservation>>,
    hub: &ObservationHub,
    update: impl FnOnce(&mut ConversationObservation),
) {
    {
        let mut observation = observation
            .lock()
            .expect("conversation observation mutex poisoned");
        update(&mut observation);
        observation.revision = observation.revision.saturating_add(1);
        observation.latest_event_at = Utc::now();
    }
    hub.publish();
}

fn apply_agent_event(
    observation: &Arc<Mutex<ConversationObservation>>,
    hub: &ObservationHub,
    event: &AgentEvent,
) {
    update_observation(observation, hub, |observation| match event {
        AgentEvent::UserMessage(message) => {
            if observation.messages.is_empty()
                && let Some(content) = message.content.as_deref()
            {
                observation.title = content.chars().take(72).collect();
            }
            if let Some(message) = observed_message(message, false) {
                observation.messages.push(message);
            }
        }
        AgentEvent::AssistantMessage(message) => {
            if let Some(message) = observed_message(message, false) {
                upsert_observed_message(&mut observation.messages, message);
            }
        }
        AgentEvent::AssistantTextDelta {
            delta,
            start,
            sequence,
            created_at,
        } => {
            let id = format!("assistant:{sequence}");
            if *start {
                observation.messages.push(ObservedMessage {
                    id,
                    role: "assistant",
                    created_at: *created_at,
                    content: delta.clone(),
                    partial: true,
                });
            } else if let Some(message) = observation
                .messages
                .iter_mut()
                .rev()
                .find(|message| message.id == id)
            {
                message.content.push_str(delta);
            }
        }
        AgentEvent::AssistantStreamReset => {
            observation
                .messages
                .retain(|message| !(message.role == "assistant" && message.partial));
        }
        AgentEvent::AssistantMessageCompleted(message) => {
            if let Some(message) = observed_message(message, false) {
                upsert_observed_message(&mut observation.messages, message);
            }
        }
        AgentEvent::Activity(activity) => {
            observation.activity = Some(observed_activity(activity));
        }
    });
}

fn reconcile_observation(
    observation: &Arc<Mutex<ConversationObservation>>,
    hub: &ObservationHub,
    snapshot: &ConversationSnapshot,
    lifecycle: ConversationLifecycle,
) {
    update_observation(observation, hub, |observation| {
        let revision = observation.revision;
        let catalog_error = observation.catalog_error.clone();
        *observation = observation_from_snapshot(snapshot, lifecycle, revision);
        observation.catalog_error = catalog_error;
    });
}

fn observation_from_snapshot(
    snapshot: &ConversationSnapshot,
    lifecycle: ConversationLifecycle,
    revision: u64,
) -> ConversationObservation {
    let session = &snapshot.session;
    let mut messages = session
        .messages
        .active_entries()
        .filter_map(|(_, message)| observed_message(message, false))
        .collect::<Vec<_>>();
    let last_turn = session.turns.last().and_then(|turn| {
        let ended_at = turn.completed_at?;
        Some(ObservedTurn {
            outcome: turn.outcome.unwrap_or(TurnOutcome::Completed),
            started_at: turn.started_at,
            ended_at,
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
        .map(|goal| ObservedGoal {
            id: goal.id,
            status: goal.status,
        });
    ConversationObservation {
        revision,
        title: session.title.clone(),
        lifecycle,
        active_turn_started_at: (lifecycle == ConversationLifecycle::Running)
            .then(|| session.turns.last().map(|turn| turn.started_at))
            .flatten(),
        latest_event_at: session.updated_at,
        catalog_error: None,
        last_turn,
        visible_goal,
        messages,
        activity: session.activities.last().map(observed_activity),
    }
}

async fn wait_for_cancellation(cancellation: &mut watch::Receiver<bool>) {
    loop {
        if *cancellation.borrow() {
            return;
        }
        if cancellation.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

fn observed_message(message: &Message, partial: bool) -> Option<ObservedMessage> {
    if message.hidden || !matches!(message.role, Role::User | Role::Assistant) {
        return None;
    }
    let created_at = message.created_at.unwrap_or_else(Utc::now);
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        _ => unreachable!("visible observation filters protocol roles"),
    };
    let id = message
        .sequence
        .map(|sequence| format!("assistant:{sequence}"))
        .unwrap_or_else(|| {
            format!(
                "{role}:{}",
                created_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            )
        });
    Some(ObservedMessage {
        id,
        role,
        created_at,
        content: message.content.clone().unwrap_or_default(),
        partial,
    })
}

fn upsert_observed_message(messages: &mut Vec<ObservedMessage>, message: ObservedMessage) {
    if let Some(existing) = messages
        .iter_mut()
        .rev()
        .find(|existing| existing.id == message.id)
    {
        *existing = message;
    } else {
        messages.push(message);
    }
}

fn observed_activity(activity: &AgentActivity) -> ObservedActivity {
    ObservedActivity {
        tool: activity.tool.clone(),
        status: activity.status,
        started_at: activity.started_at,
        completed_at: activity.completed_at,
    }
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
    observation_hub: ObservationHub,
}

impl ConversationManager {
    pub(crate) fn new(registry: SessionRegistry) -> Self {
        Self {
            conversations: Arc::new(RwLock::new(HashMap::new())),
            registry,
            observation_hub: ObservationHub::default(),
        }
    }

    #[cfg(test)]
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
        let handle = ConversationHandle::spawn_with_hub(
            agent,
            self.registry.clone(),
            self.observation_hub.clone(),
        )?;
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

    pub(crate) fn observation_hub(&self) -> ObservationHub {
        self.observation_hub.clone()
    }

    pub(crate) async fn shutdown_all(&self) -> Result<Vec<ConversationSnapshot>> {
        self.cancel_all();
        while self
            .conversations
            .read()
            .expect("conversation manager lock poisoned")
            .values()
            .any(ConversationHandle::is_running)
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
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
        model_catalog: agent.model_catalog().to_vec(),
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
            root.join(".test-global-config").join("AGENTS.md"),
            crate::diagnostics::DiagnosticLog::default(),
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
