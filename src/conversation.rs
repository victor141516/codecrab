use std::{
    collections::{HashMap, VecDeque},
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
    sync::{broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
};
use uuid::Uuid;

use crate::{
    agent::{Agent, turn_was_cancelled},
    attachments::Attachment,
    config::SessionRegistry,
    events::{ActivityStatus, AgentActivity, AgentEvent},
    provider::{AttachmentBinding, Message, ModelCatalogEntry, ModelSelection, Role},
    session::{GoalStatus, Session, SessionStore, TurnOutcome},
    skills::{Skill, SkillRegistry},
    terminal::{TerminalManager, TerminalOutputSnapshot, TerminalRecord},
};

#[derive(Clone)]
pub(crate) struct ConversationSnapshot {
    pub project_root: PathBuf,
    pub session: Session,
    pub skills: Vec<ConversationSkill>,
    pub model_catalog: Vec<ModelCatalogEntry>,
}

#[derive(Clone)]
pub(crate) struct ConversationLiveState {
    pub snapshot: ConversationSnapshot,
    pub observation: ConversationObservation,
    pub lifecycle: ConversationLifecycle,
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

const LIVE_EVENT_BUFFER: usize = 1_024;

#[derive(Clone)]
pub(crate) struct ConversationLiveEnvelope {
    pub revision: u64,
    pub session_id: Uuid,
    pub project_root: PathBuf,
    pub observation_revision: u64,
    pub event: ConversationLiveEvent,
}

#[derive(Clone)]
pub(crate) enum ConversationLiveEvent {
    Installed,
    Removed,
    Lifecycle,
    Agent(AgentEvent),
    Snapshot,
    Terminals,
}

#[derive(Clone)]
pub(crate) struct ConversationLiveHub {
    revision: Arc<AtomicU64>,
    events: broadcast::Sender<ConversationLiveEnvelope>,
}

impl Default for ConversationLiveHub {
    fn default() -> Self {
        let (events, _) = broadcast::channel(LIVE_EVENT_BUFFER);
        Self {
            revision: Arc::new(AtomicU64::new(0)),
            events,
        }
    }
}

impl ConversationLiveHub {
    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ConversationLiveEnvelope> {
        self.events.subscribe()
    }

    fn publish(
        &self,
        session_id: Uuid,
        project_root: PathBuf,
        observation_revision: u64,
        event: ConversationLiveEvent,
    ) {
        let revision = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.events.send(ConversationLiveEnvelope {
            revision,
            session_id,
            project_root,
            observation_revision,
            event,
        });
    }
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

#[derive(Clone, Serialize)]
pub(crate) struct ObservedMessage {
    pub id: String,
    pub role: &'static str,
    pub sequence: Option<u64>,
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

#[derive(Clone, Serialize)]
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
    pub display_messages: Vec<Message>,
    pub activities: Vec<AgentActivity>,
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
    terminals: TerminalManager,
    session_id: Uuid,
    project_root: PathBuf,
    live_hub: ConversationLiveHub,
    skill_refresh: Arc<Mutex<SkillRefreshCache>>,
}

const RECENT_SKILL_REFRESH_IDS: usize = 64;

struct SkillRefreshCache {
    recent: VecDeque<(Uuid, Vec<ConversationSkill>)>,
}

enum ConversationCommand {
    Turn {
        prompt: String,
        attachments: Vec<AttachmentBinding>,
        hidden: bool,
        edit_node_id: Option<Uuid>,
        initialize_catalog: bool,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
        cancellation: watch::Receiver<bool>,
        reply: oneshot::Sender<ConversationTurn>,
    },
    AddAttachment {
        attachment: Attachment,
        reply: oneshot::Sender<Result<(ConversationSnapshot, Attachment)>>,
    },
    SetModel {
        selection: ModelSelection,
        reply: oneshot::Sender<Result<ConversationSnapshot>>,
    },
    ReplaceSkills {
        skills: SkillRegistry,
        reply: Option<oneshot::Sender<ConversationSnapshot>>,
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
    session_id: Uuid,
    project_root: PathBuf,
    live_hub: ConversationLiveHub,
}

impl ConversationHandle {
    #[cfg(test)]
    pub(crate) fn spawn(agent: Agent, registry: SessionRegistry) -> Result<Self> {
        Self::spawn_with_hubs(
            agent,
            registry,
            ObservationHub::default(),
            ConversationLiveHub::default(),
        )
    }

    fn spawn_with_hubs(
        agent: Agent,
        registry: SessionRegistry,
        observation_hub: ObservationHub,
        live_hub: ConversationLiveHub,
    ) -> Result<Self> {
        let initial = snapshot(&agent);
        let terminals = agent.terminal_manager();
        let session_id = initial.session.id;
        let project_root = initial.project_root.clone();
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
        let skill_refresh = Arc::new(Mutex::new(SkillRefreshCache {
            recent: VecDeque::new(),
        }));
        let mut terminal_changes = terminals.subscribe();
        let terminal_state = terminals.clone();
        let terminal_live_hub = live_hub.clone();
        let terminal_observation = observation.clone();
        let terminal_project_root = project_root.clone();
        spawn_worker(async move {
            let mut active_count = terminal_state.running_records().len();
            while terminal_changes.changed().await.is_ok() {
                let next_active_count = terminal_state.running_records().len();
                if next_active_count == active_count {
                    continue;
                }
                active_count = next_active_count;
                let observation_revision = terminal_observation
                    .lock()
                    .expect("conversation observation mutex poisoned")
                    .revision;
                terminal_live_hub.publish(
                    session_id,
                    terminal_project_root.clone(),
                    observation_revision,
                    ConversationLiveEvent::Terminals,
                );
            }
        })?;
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
                session_id,
                project_root: project_root.clone(),
                live_hub: live_hub.clone(),
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
            terminals,
            session_id,
            project_root,
            live_hub,
            skill_refresh,
        })
    }

    pub(crate) fn snapshot(&self) -> ConversationSnapshot {
        self.snapshot.borrow().clone()
    }

    fn snapshot_with_terminal_state(&self) -> ConversationSnapshot {
        let mut snapshot = self.snapshot();
        let (next_terminal_id, terminals) = self.terminals.persisted_state();
        snapshot.session.next_terminal_id = next_terminal_id;
        snapshot.session.terminals = terminals;
        snapshot
    }

    pub(crate) fn running_terminals(&self) -> Vec<TerminalRecord> {
        self.terminals.running_records()
    }

    pub(crate) fn running_terminal_count(&self) -> usize {
        self.terminals.running_records().len()
    }

    pub(crate) fn terminal_output(&self, terminal_id: &str) -> Result<TerminalOutputSnapshot> {
        self.terminals.output(terminal_id)
    }

    pub(crate) fn close_terminal(&self, terminal_id: &str) -> Result<()> {
        self.terminals.close(terminal_id).map(|_| ())
    }

    pub(crate) fn close_terminals(&self) -> Result<()> {
        self.terminals.close_all()
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

    pub(crate) fn live_state(&self) -> ConversationLiveState {
        ConversationLiveState {
            snapshot: self.snapshot_with_terminal_state(),
            observation: self.observation(),
            lifecycle: self.lifecycle(),
        }
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
            let observation_revision =
                update_observation(&self.observation, &self.observation_hub, |observation| {
                    observation.lifecycle = ConversationLifecycle::Stopping;
                });
            self.live_hub.publish(
                self.session_id,
                self.project_root.clone(),
                observation_revision,
                ConversationLiveEvent::Lifecycle,
            );
        }
        requested
    }

    pub(crate) fn start_turn(
        &self,
        prompt: String,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_with_attachments(prompt, Vec::new(), events)
    }

    pub(crate) fn start_turn_with_attachments(
        &self,
        prompt: String,
        attachments: Vec<AttachmentBinding>,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_kind(prompt, attachments, false, None, false, events)
    }

    pub(crate) fn start_new_session_turn(
        &self,
        prompt: String,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_kind(prompt, Vec::new(), false, None, true, None)
    }

    pub(crate) fn start_edit_turn_with_attachments(
        &self,
        node_id: Uuid,
        prompt: String,
        attachments: Vec<AttachmentBinding>,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_kind(prompt, attachments, false, Some(node_id), false, events)
    }

    pub(crate) fn start_goal_continuation(
        &self,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<JoinHandle<Result<ConversationTurn>>> {
        self.start_turn_kind(String::new(), Vec::new(), true, None, false, events)
    }

    fn start_turn_kind(
        &self,
        prompt: String,
        attachments: Vec<AttachmentBinding>,
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
        let observation_revision =
            update_observation(&self.observation, &self.observation_hub, |observation| {
                observation.lifecycle = ConversationLifecycle::Running;
                observation.active_turn_started_at = Some(Utc::now());
                observation.activity = None;
            });
        self.live_hub.publish(
            self.session_id,
            self.project_root.clone(),
            observation_revision,
            ConversationLiveEvent::Lifecycle,
        );
        let (reply, response) = oneshot::channel();
        if let Err(error) = self.send_unchecked(ConversationCommand::Turn {
            prompt,
            attachments,
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
            let observation_revision =
                update_observation(&self.observation, &self.observation_hub, |observation| {
                    observation.lifecycle = ConversationLifecycle::Idle;
                    observation.active_turn_started_at = None;
                });
            self.live_hub.publish(
                self.session_id,
                self.project_root.clone(),
                observation_revision,
                ConversationLiveEvent::Lifecycle,
            );
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

    #[cfg(test)]
    pub(crate) async fn refresh_skills(&self) -> Result<Vec<ConversationSkill>> {
        let (skills, view) = self.discover_skills();
        let (reply, response) = oneshot::channel();
        self.send(ConversationCommand::ReplaceSkills {
            skills,
            reply: Some(reply),
        })?;
        receive(response, "refreshing conversation skills").await?;
        Ok(view)
    }

    pub(crate) fn queue_skill_refresh_once(
        &self,
        refresh_id: Uuid,
    ) -> Result<Vec<ConversationSkill>> {
        let mut cache = self
            .skill_refresh
            .lock()
            .expect("conversation skill refresh mutex poisoned");
        if let Some((_, skills)) = cache.recent.iter().find(|(id, _)| *id == refresh_id) {
            return Ok(skills.clone());
        }
        let (skills, view) = self.discover_skills();
        self.send(ConversationCommand::ReplaceSkills {
            skills,
            reply: None,
        })?;
        if cache.recent.len() == RECENT_SKILL_REFRESH_IDS {
            cache.recent.pop_front();
        }
        cache.recent.push_back((refresh_id, view.clone()));
        Ok(view)
    }

    fn discover_skills(&self) -> (SkillRegistry, Vec<ConversationSkill>) {
        let skills = SkillRegistry::discover(&self.project_root);
        let view = conversation_skills(skills.skills());
        (skills, view)
    }

    pub(crate) async fn add_attachment(
        &self,
        attachment: Attachment,
    ) -> Result<(ConversationSnapshot, Attachment)> {
        let (reply, response) = oneshot::channel();
        self.send(ConversationCommand::AddAttachment { attachment, reply })?;
        receive(response, "adding a session attachment").await?
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
        session_id,
        project_root,
        live_hub,
    } = state;
    let mut explicitly_stopped = false;
    while let Some(command) = commands.recv().await {
        let stop = match command {
            ConversationCommand::Turn {
                prompt,
                attachments,
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
                let live_observation_hub = observation_hub.clone();
                let event_hub = live_hub.clone();
                let event_project_root = project_root.clone();
                let forwarded_events = events.clone();
                let observer = tokio::spawn(async move {
                    while let Some(event) = event_rx.recv().await {
                        let observation_revision =
                            apply_agent_event(&live_observation, &live_observation_hub, &event);
                        event_hub.publish(
                            session_id,
                            event_project_root.clone(),
                            observation_revision,
                            ConversationLiveEvent::Agent(event.clone()),
                        );
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
                        .edit_turn_with_attachments_controlled(
                            node_id,
                            &prompt,
                            &attachments,
                            Some(internal_events),
                            cancellation,
                        )
                        .await
                } else if hidden {
                    agent
                        .continue_goal_with_events(internal_events, cancellation)
                        .await
                } else {
                    agent
                        .turn_with_attachments_controlled(
                            &prompt,
                            &attachments,
                            Some(internal_events),
                            cancellation,
                        )
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
                let observation_revision = reconcile_observation(
                    &observation,
                    &observation_hub,
                    &current,
                    terminal_lifecycle,
                );
                live_hub.publish(
                    session_id,
                    project_root.clone(),
                    observation_revision,
                    ConversationLiveEvent::Snapshot,
                );
                let _ = reply.send(ConversationTurn {
                    result,
                    snapshot: current,
                });
                false
            }
            ConversationCommand::AddAttachment { attachment, reply } => {
                let attachment = agent.add_attachment(attachment);
                let current = snapshot(&agent);
                let _ = snapshots.send(current.clone());
                let result = persist(&agent, &registry).map(|()| (current, attachment));
                let _ = reply.send(result);
                false
            }
            ConversationCommand::SetModel { selection, reply } => {
                agent.set_model_selection(selection);
                reply_snapshot(&agent, &registry, &snapshots, reply);
                false
            }
            ConversationCommand::ReplaceSkills { skills, reply } => {
                agent.replace_skills(skills);
                let current = snapshot(&agent);
                let _ = snapshots.send(current.clone());
                if let Some(reply) = reply {
                    let _ = reply.send(current);
                }
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
    let observation_revision = update_observation(&observation, &observation_hub, |observation| {
        observation.lifecycle = if explicitly_stopped {
            ConversationLifecycle::Stopped
        } else {
            ConversationLifecycle::Failed
        };
        observation.active_turn_started_at = None;
    });
    live_hub.publish(
        session_id,
        project_root,
        observation_revision,
        ConversationLiveEvent::Lifecycle,
    );
}

fn update_observation(
    observation: &Arc<Mutex<ConversationObservation>>,
    hub: &ObservationHub,
    update: impl FnOnce(&mut ConversationObservation),
) -> u64 {
    let revision = {
        let mut observation = observation
            .lock()
            .expect("conversation observation mutex poisoned");
        update(&mut observation);
        observation.revision = observation.revision.saturating_add(1);
        observation.latest_event_at = Utc::now();
        observation.revision
    };
    hub.publish();
    revision
}

fn apply_agent_event(
    observation: &Arc<Mutex<ConversationObservation>>,
    hub: &ObservationHub,
    event: &AgentEvent,
) -> u64 {
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
            observation.display_messages.push(message.clone());
        }
        AgentEvent::AssistantMessage(message) => {
            if let Some(message) = observed_message(message, false) {
                upsert_observed_message(&mut observation.messages, message);
            }
            upsert_display_message(&mut observation.display_messages, message.clone());
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
                    sequence: Some(*sequence),
                    created_at: *created_at,
                    content: delta.clone(),
                    partial: true,
                });
                observation.display_messages.push(Message {
                    role: Role::Assistant,
                    sequence: Some(*sequence),
                    created_at: Some(*created_at),
                    content: Some(delta.clone()),
                    parts: Vec::new(),
                    tool_calls: None,
                    tool_call_id: None,
                    hidden: false,
                });
            } else if let Some(message) = observation
                .messages
                .iter_mut()
                .rev()
                .find(|message| message.id == id)
            {
                message.content.push_str(delta);
            }
            if !*start
                && let Some(message) = observation
                    .display_messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.sequence == Some(*sequence))
            {
                message.content.get_or_insert_default().push_str(delta);
            }
        }
        AgentEvent::AssistantStreamReset => {
            let partial_sequences = observation
                .messages
                .iter()
                .filter(|message| message.role == "assistant" && message.partial)
                .filter_map(|message| message.sequence)
                .collect::<Vec<_>>();
            observation
                .messages
                .retain(|message| !(message.role == "assistant" && message.partial));
            observation.display_messages.retain(|message| {
                message
                    .sequence
                    .is_none_or(|sequence| !partial_sequences.contains(&sequence))
            });
        }
        AgentEvent::AssistantMessageCompleted(message) => {
            if let Some(message) = observed_message(message, false) {
                upsert_observed_message(&mut observation.messages, message);
            }
            upsert_display_message(&mut observation.display_messages, message.clone());
        }
        AgentEvent::Activity(activity) => {
            observation.activity = Some(observed_activity(activity));
            if let Some(existing) = observation
                .activities
                .iter_mut()
                .find(|existing| existing.id == activity.id)
            {
                existing.clone_from(activity);
            } else {
                observation.activities.push(activity.clone());
            }
        }
    })
}

fn reconcile_observation(
    observation: &Arc<Mutex<ConversationObservation>>,
    hub: &ObservationHub,
    snapshot: &ConversationSnapshot,
    lifecycle: ConversationLifecycle,
) -> u64 {
    update_observation(observation, hub, |observation| {
        let revision = observation.revision;
        let catalog_error = observation.catalog_error.clone();
        *observation = observation_from_snapshot(snapshot, lifecycle, revision);
        observation.catalog_error = catalog_error;
    })
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
        display_messages: session
            .messages
            .active_entries()
            .map(|(_, message)| message.clone())
            .collect(),
        activities: session.activities.clone(),
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
        sequence: message.sequence,
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

fn upsert_display_message(messages: &mut Vec<Message>, message: Message) {
    let existing = message.sequence.and_then(|sequence| {
        messages
            .iter_mut()
            .rev()
            .find(|existing| existing.sequence == Some(sequence))
    });
    if let Some(existing) = existing {
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
    live_hub: ConversationLiveHub,
}

impl ConversationManager {
    pub(crate) fn new(registry: SessionRegistry) -> Self {
        Self {
            conversations: Arc::new(RwLock::new(HashMap::new())),
            registry,
            observation_hub: ObservationHub::default(),
            live_hub: ConversationLiveHub::default(),
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
        let handle = ConversationHandle::spawn_with_hubs(
            agent,
            self.registry.clone(),
            self.observation_hub.clone(),
            self.live_hub.clone(),
        )?;
        let observation_revision = handle.observation().revision;
        let project_root = handle.snapshot().project_root;
        conversations.insert(id, handle.clone());
        drop(conversations);
        self.live_hub.publish(
            id,
            project_root,
            observation_revision,
            ConversationLiveEvent::Installed,
        );
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
        let removed = conversations.remove(&id);
        drop(conversations);
        if let Some(handle) = &removed {
            let state = handle.live_state();
            self.live_hub.publish(
                id,
                state.snapshot.project_root,
                state.observation.revision,
                ConversationLiveEvent::Removed,
            );
        }
        Ok(removed)
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

    pub(crate) fn active_terminal_count(&self) -> usize {
        self.conversations
            .read()
            .expect("conversation manager lock poisoned")
            .values()
            .map(ConversationHandle::running_terminal_count)
            .sum()
    }

    pub(crate) fn close_all_terminals(&self) -> Result<()> {
        let handles = self
            .conversations
            .read()
            .expect("conversation manager lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut first_error = None;
        for handle in handles {
            if let Err(error) = handle.close_terminals()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn observation_hub(&self) -> ObservationHub {
        self.observation_hub.clone()
    }

    pub(crate) fn live_revision(&self) -> u64 {
        self.live_hub.revision()
    }

    pub(crate) fn subscribe_live(&self) -> broadcast::Receiver<ConversationLiveEnvelope> {
        self.live_hub.subscribe()
    }

    pub(crate) fn live_states(&self) -> Vec<ConversationLiveState> {
        let handles = self
            .conversations
            .read()
            .expect("conversation manager lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.live_state())
            .collect()
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
        skills: conversation_skills(agent.skills()),
        model_catalog: agent.model_catalog().to_vec(),
    }
}

fn conversation_skills(skills: &[Skill]) -> Vec<ConversationSkill> {
    skills
        .iter()
        .map(|skill| ConversationSkill {
            name: skill.name.clone(),
            description: skill.description.clone(),
            scope: skill.scope.label(),
        })
        .collect()
}

fn persist(agent: &Agent, registry: &SessionRegistry) -> Result<()> {
    let root = agent.project_root();
    SessionStore::for_project_root_in(
        (agent.session().scope == crate::session::SessionScope::Project).then_some(root),
        &registry.data_dir()?,
    )?
    .save(agent.session())?;
    if agent.session().scope == crate::session::SessionScope::Project {
        registry.register(root)?;
    }
    Ok(())
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
    use std::{fs, path::Path};

    use serde_json::json;
    use tokio::{io::AsyncWriteExt, net::TcpListener};

    use super::*;
    use crate::{
        attachments::AttachmentStore, config::Config, provider::OpenAiCompatible,
        skills::SkillRegistry, tools::ToolBox,
    };

    fn test_agent(root: &Path) -> Agent {
        let config = Config::test("model", "http://127.0.0.1:1/v1");
        let provider = OpenAiCompatible::new(&config, &config.active_provider).unwrap();
        let session = SessionStore::new(root)
            .unwrap()
            .create_for_provider(config.active_provider.clone(), "model".into())
            .unwrap();
        Agent::new(
            provider,
            ToolBox::new(root.to_path_buf()),
            SkillRegistry::default(),
            session,
            root.join(".test-global-config").join("AGENTS.md"),
            crate::diagnostics::DiagnosticLog::default(),
        )
        .unwrap()
    }

    fn test_handle(root: &Path) -> ConversationHandle {
        let agent = test_agent(root);
        ConversationHandle::spawn(
            agent,
            SessionRegistry::at(root.join("test-global-config.toml")),
        )
        .unwrap()
    }

    #[test]
    fn live_observation_preserves_messages_and_complete_activities() {
        let root = tempfile::tempdir().unwrap();
        let agent = test_agent(root.path());
        let snapshot = snapshot(&agent);
        let observation = Arc::new(Mutex::new(observation_from_snapshot(
            &snapshot,
            ConversationLifecycle::Running,
            0,
        )));
        let hub = ObservationHub::default();
        let user = Message::text(Role::User, "Delegated work");
        apply_agent_event(&observation, &hub, &AgentEvent::UserMessage(user.clone()));
        let mut first = AgentActivity::started(
            "call-1".into(),
            Uuid::new_v4(),
            0,
            1,
            "read_file",
            r#"{"path":"src/main.rs"}"#,
        );
        apply_agent_event(&observation, &hub, &AgentEvent::Activity(first.clone()));
        first.finish(true);
        apply_agent_event(&observation, &hub, &AgentEvent::Activity(first.clone()));
        let second = AgentActivity::started(
            "call-2".into(),
            Uuid::new_v4(),
            0,
            2,
            "search",
            r#"{"query":"needle","path":"src"}"#,
        );
        apply_agent_event(&observation, &hub, &AgentEvent::Activity(second.clone()));

        let observation = observation.lock().unwrap();
        assert_eq!(observation.messages.len(), 1);
        assert_eq!(
            observation.messages[0].content,
            user.content.as_deref().unwrap()
        );
        assert_eq!(observation.activities.len(), 2);
        assert_eq!(observation.activities[0].id, first.id);
        assert_eq!(observation.activities[0].status, ActivityStatus::Completed);
        assert_eq!(observation.activities[0].detail, "src/main.rs");
        assert_eq!(observation.activities[1].id, second.id);
        assert_eq!(observation.activities[1].status, ActivityStatus::Running);
    }

    #[tokio::test]
    async fn manager_publishes_install_and_removal_events() {
        let root = tempfile::tempdir().unwrap();
        let registry = SessionRegistry::at(root.path().join("manager-global-config.toml"));
        let manager = ConversationManager::new(registry);
        let mut events = manager.subscribe_live();
        let handle = manager.install(test_agent(root.path())).unwrap();
        let id = handle.snapshot().session.id;

        let installed = events.recv().await.unwrap();
        assert_eq!(installed.session_id, id);
        assert!(matches!(installed.event, ConversationLiveEvent::Installed));

        assert!(manager.take_if_idle(id).unwrap().is_some());
        let removed = events.recv().await.unwrap();
        assert_eq!(removed.session_id, id);
        assert!(matches!(removed.event, ConversationLiveEvent::Removed));
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
    async fn refreshing_skills_replaces_the_active_worker_registry() {
        let root = tempfile::tempdir().unwrap();
        let handle = test_handle(root.path());
        assert!(handle.snapshot().skills.is_empty());

        let skill = root.path().join(".agents/skills/review-rust");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: review-rust\ndescription: Review Rust changes.\n---\nReview the code.",
        )
        .unwrap();

        let refreshed = handle.refresh_skills().await.unwrap();
        let refreshed_skill = refreshed
            .iter()
            .find(|skill| skill.name == "review-rust")
            .unwrap();
        assert_eq!(refreshed_skill.description, "Review Rust changes.");

        let snapshot = handle.snapshot();
        assert!(
            snapshot
                .skills
                .iter()
                .any(|skill| skill.name == "review-rust")
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn refreshed_skill_lifecycle_controls_the_next_turn() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, mut request_rx) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            for _ in 0..5 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = crate::test_support::read_http_request(&mut socket).await;
                request_tx.send(request).unwrap();
                let body = json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "Done."}
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

        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join(".agents/skills/issue-62-review");
        fs::create_dir_all(&skill).unwrap();
        let config = Config::test("model", format!("http://{address}/v1"));
        let provider = OpenAiCompatible::new(&config, &config.active_provider).unwrap();
        let session = SessionStore::new(root.path())
            .unwrap()
            .create_for_provider(config.active_provider.clone(), "model".into())
            .unwrap();
        let agent = Agent::new(
            provider,
            ToolBox::new(root.path().to_path_buf()),
            SkillRegistry::discover(root.path()),
            session,
            root.path().join(".test-global-config/AGENTS.md"),
            crate::diagnostics::DiagnosticLog::default(),
        )
        .unwrap();
        let handle = ConversationHandle::spawn(
            agent,
            SessionRegistry::at(root.path().join("test-global-config.toml")),
        )
        .unwrap();

        fs::write(
            skill.join("SKILL.md"),
            "---\nname: issue-62-review\ndescription: Review the new change.\n---\nADDED_SKILL_INSTRUCTIONS",
        )
        .unwrap();
        handle.refresh_skills().await.unwrap();
        handle.turn("Use /issue-62-review".into()).await.unwrap();
        let added_request = String::from_utf8(request_rx.recv().await.unwrap()).unwrap();
        assert!(added_request.contains("ADDED_SKILL_INSTRUCTIONS"));

        fs::write(
            skill.join("SKILL.md"),
            "---\nname: issue-62-review\ndescription: Review the updated change.\n---\nUPDATED_SKILL_INSTRUCTIONS",
        )
        .unwrap();
        handle.refresh_skills().await.unwrap();
        handle.turn("Use /issue-62-review".into()).await.unwrap();
        let updated_request = String::from_utf8(request_rx.recv().await.unwrap()).unwrap();
        assert!(updated_request.contains("UPDATED_SKILL_INSTRUCTIONS"));
        assert!(!updated_request.contains("ADDED_SKILL_INSTRUCTIONS"));

        fs::write(
            skill.join("SKILL.md"),
            "---\nname: mismatched-skill\ndescription: Invalid.\n---\nINVALID_SKILL_INSTRUCTIONS",
        )
        .unwrap();
        handle.refresh_skills().await.unwrap();
        handle.turn("Use /issue-62-review".into()).await.unwrap();
        let invalid_request = String::from_utf8(request_rx.recv().await.unwrap()).unwrap();
        assert!(!invalid_request.contains("UPDATED_SKILL_INSTRUCTIONS"));
        assert!(!invalid_request.contains("INVALID_SKILL_INSTRUCTIONS"));

        fs::write(
            skill.join("SKILL.md"),
            "---\nname: issue-62-review\ndescription: Restored skill.\n---\nRESTORED_SKILL_INSTRUCTIONS",
        )
        .unwrap();
        handle.refresh_skills().await.unwrap();
        handle.turn("Use /issue-62-review".into()).await.unwrap();
        let restored_request = String::from_utf8(request_rx.recv().await.unwrap()).unwrap();
        assert!(restored_request.contains("RESTORED_SKILL_INSTRUCTIONS"));

        fs::remove_file(skill.join("SKILL.md")).unwrap();
        handle.refresh_skills().await.unwrap();
        handle.turn("Use /issue-62-review".into()).await.unwrap();
        let deleted_request = String::from_utf8(request_rx.recv().await.unwrap()).unwrap();
        assert!(!deleted_request.contains("RESTORED_SKILL_INSTRUCTIONS"));

        handle.shutdown().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_attachment_additions_deduplicate_in_the_session_worker() {
        let root = tempfile::tempdir().unwrap();
        let handle = test_handle(root.path());
        let session_id = handle.snapshot().session.id;
        let source = root.path().join("same.txt");
        fs::write(&source, b"same bytes").unwrap();
        let store = AttachmentStore::new(root.path());
        let first = store.import_path(session_id, &[], &source).unwrap();
        let second = store.import_path(session_id, &[], &source).unwrap();
        assert_ne!(first.id, second.id);

        let (first_result, second_result) =
            tokio::join!(handle.add_attachment(first), handle.add_attachment(second));
        let (_, first) = first_result.unwrap();
        let (_, second) = second_result.unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(handle.snapshot().session.attachments.len(), 1);
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
