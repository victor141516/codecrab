use std::{
    collections::{HashMap, HashSet},
    fs,
    ops::Deref,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    attachments::{Attachment, AttachmentStore},
    config::{SessionRegistry, normalized_root, paths_equal},
    events::AgentActivity,
    provider::{Message, Role, TokenUsage},
    terminal::TerminalRecord,
};

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct ConversationNode {
    pub id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Uuid>,
    pub message: Message,
}

#[derive(Clone, Copy, Serialize)]
pub(crate) struct ConversationGraphNode {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
}

#[derive(Clone, Default, Serialize)]
pub(crate) struct ConversationTree {
    nodes: Vec<ConversationNode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_leaf_id: Option<Uuid>,
    #[serde(skip)]
    active_node_ids: Vec<Uuid>,
    #[serde(skip)]
    active_messages: Vec<Message>,
}

#[derive(Deserialize)]
struct PersistedConversationTree {
    nodes: Vec<ConversationNode>,
    #[serde(default)]
    active_leaf_id: Option<Uuid>,
}

impl<'de> Deserialize<'de> for ConversationTree {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let persisted = PersistedConversationTree::deserialize(deserializer)?;
        let mut tree = Self {
            nodes: persisted.nodes,
            active_leaf_id: persisted.active_leaf_id,
            active_node_ids: Vec::new(),
            active_messages: Vec::new(),
        };
        tree.rebuild_active_path()
            .map_err(serde::de::Error::custom)?;
        Ok(tree)
    }
}

impl Deref for ConversationTree {
    type Target = [Message];

    fn deref(&self) -> &Self::Target {
        &self.active_messages
    }
}

impl<'a> IntoIterator for &'a ConversationTree {
    type Item = &'a Message;
    type IntoIter = std::slice::Iter<'a, Message>;

    fn into_iter(self) -> Self::IntoIter {
        self.active_messages.iter()
    }
}

impl ConversationTree {
    pub(crate) fn active_leaf_id(&self) -> Option<Uuid> {
        self.active_leaf_id
    }

    pub(crate) fn active_node_ids(&self) -> &[Uuid] {
        &self.active_node_ids
    }

    pub(crate) fn active_entries(&self) -> impl Iterator<Item = (Uuid, &Message)> {
        self.active_node_ids
            .iter()
            .copied()
            .zip(self.active_messages.iter())
    }

    pub(crate) fn active_node_id(&self, message_index: usize) -> Option<Uuid> {
        self.active_node_ids.get(message_index).copied()
    }

    pub(crate) fn message(&self, id: Uuid) -> Option<&Message> {
        self.nodes
            .iter()
            .find(|node| node.id == id)
            .map(|node| &node.message)
    }

    pub(crate) fn visible_user_nodes(&self) -> Vec<ConversationGraphNode> {
        let visible = self
            .nodes
            .iter()
            .filter(|node| matches!(node.message.role, Role::User) && !node.message.hidden)
            .map(|node| node.id)
            .collect::<HashSet<_>>();
        let parents = self
            .nodes
            .iter()
            .map(|node| (node.id, node.parent_id))
            .collect::<HashMap<_, _>>();
        self.nodes
            .iter()
            .filter(|node| visible.contains(&node.id))
            .map(|node| {
                let mut parent_id = node.parent_id;
                while parent_id.is_some_and(|id| !visible.contains(&id)) {
                    parent_id = parent_id.and_then(|id| parents.get(&id)).copied().flatten();
                }
                ConversationGraphNode {
                    id: node.id,
                    parent_id,
                }
            })
            .collect()
    }

    pub(crate) fn push(&mut self, message: Message) -> Uuid {
        self.branch_from(self.active_leaf_id, message)
            .expect("the active conversation leaf must exist")
    }

    pub(crate) fn branch_from(
        &mut self,
        parent_id: Option<Uuid>,
        message: Message,
    ) -> Result<Uuid> {
        if let Some(parent_id) = parent_id
            && !self.nodes.iter().any(|node| node.id == parent_id)
        {
            anyhow::bail!("conversation node {parent_id} does not exist");
        }
        let id = Uuid::new_v4();
        let extends_active_path = parent_id == self.active_leaf_id;
        let active_message = extends_active_path.then(|| message.clone());
        self.nodes.push(ConversationNode {
            id,
            parent_id,
            message,
        });
        self.active_leaf_id = Some(id);
        if let Some(message) = active_message {
            self.active_node_ids.push(id);
            self.active_messages.push(message);
        } else {
            self.rebuild_active_path().map_err(anyhow::Error::msg)?;
        }
        Ok(id)
    }

    pub(crate) fn is_ancestor(&self, ancestor: Uuid, descendant: Uuid) -> bool {
        let parents = self
            .nodes
            .iter()
            .map(|node| (node.id, node.parent_id))
            .collect::<HashMap<_, _>>();
        let mut current = Some(descendant);
        while let Some(id) = current {
            if id == ancestor {
                return true;
            }
            current = parents.get(&id).copied().flatten();
        }
        false
    }

    fn select_from_node(&mut self, id: Uuid) -> Result<Uuid> {
        if !self.nodes.iter().any(|node| node.id == id) {
            anyhow::bail!("conversation node {id} does not exist");
        }
        let mut leaf_id = id;
        while let Some(child) = self
            .nodes
            .iter()
            .find(|node| node.parent_id == Some(leaf_id))
        {
            leaf_id = child.id;
        }
        self.active_leaf_id = Some(leaf_id);
        self.rebuild_active_path().map_err(anyhow::Error::msg)?;
        Ok(leaf_id)
    }

    fn branch_from_edited_user_message(&mut self, id: Uuid, message: Message) -> Result<Uuid> {
        let node = self
            .nodes
            .iter()
            .find(|node| node.id == id)
            .context(format!("conversation node {id} does not exist"))?;
        if !matches!(node.message.role, Role::User) || node.message.hidden {
            anyhow::bail!("only visible user messages can be edited");
        }
        let parent_id = node.parent_id;
        self.branch_from(parent_id, message)
    }

    fn rebuild_active_path(&mut self) -> std::result::Result<(), String> {
        let mut indexes = HashMap::with_capacity(self.nodes.len());
        for (index, node) in self.nodes.iter().enumerate() {
            if indexes.insert(node.id, index).is_some() {
                return Err(format!("duplicate conversation node id {}", node.id));
            }
        }
        for node in &self.nodes {
            if let Some(parent_id) = node.parent_id
                && !indexes.contains_key(&parent_id)
            {
                return Err(format!(
                    "conversation node {} references missing parent {parent_id}",
                    node.id
                ));
            }
        }
        for node in &self.nodes {
            let mut visited = HashSet::new();
            let mut current = Some(node.id);
            while let Some(id) = current {
                if !visited.insert(id) {
                    return Err(format!("conversation tree contains a cycle at node {id}"));
                }
                current = indexes
                    .get(&id)
                    .and_then(|index| self.nodes[*index].parent_id);
            }
        }

        if self.nodes.is_empty() {
            if self.active_leaf_id.is_some() {
                return Err("empty conversation tree has an active leaf".into());
            }
            self.active_node_ids.clear();
            self.active_messages.clear();
            return Ok(());
        }

        let active_leaf_id = self
            .active_leaf_id
            .ok_or_else(|| "non-empty conversation tree has no active leaf".to_owned())?;
        if self
            .nodes
            .iter()
            .any(|node| node.parent_id == Some(active_leaf_id))
        {
            return Err(format!(
                "active conversation node {active_leaf_id} is not a leaf"
            ));
        }

        let mut path = Vec::new();
        let mut visited = HashSet::new();
        let mut current = Some(active_leaf_id);
        while let Some(id) = current {
            if !visited.insert(id) {
                return Err(format!("conversation tree contains a cycle at node {id}"));
            }
            let index = indexes
                .get(&id)
                .copied()
                .ok_or_else(|| format!("active conversation leaf {id} does not exist"))?;
            let node = &self.nodes[index];
            path.push(index);
            current = node.parent_id;
        }
        path.reverse();

        self.active_node_ids = path.iter().map(|index| self.nodes[*index].id).collect();
        self.active_messages = path
            .into_iter()
            .map(|index| self.nodes[index].message.clone())
            .collect();
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GoalStatus {
    Active,
    Paused,
    Completed,
    Blocked,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Goal {
    pub id: Uuid,
    pub objective: String,
    pub status: GoalStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct AgentTurn {
    pub message_id: Uuid,
    pub message_index: usize,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TurnOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileChangeKind {
    Operation,
    Turn,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct FileChangeSet {
    pub id: Uuid,
    pub turn_message_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_id: Option<String>,
    pub kind: FileChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TurnOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    pub files: Vec<FileChange>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "storage", rename_all = "snake_case")]
pub(crate) enum FileChange {
    Git(GitFileChange),
    Temporary(TemporaryFileChange),
}

impl FileChange {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Git(change) => &change.path,
            Self::Temporary(change) => &change.path,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct GitFileChange {
    pub path: PathBuf,
    pub commit: String,
    pub relative_path: PathBuf,
    pub before_patch: PathBuf,
    pub after_patch: PathBuf,
    pub before_sha256: String,
    pub after_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TemporaryFileChange {
    pub path: PathBuf,
    pub before: PathBuf,
    pub after: PathBuf,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TurnOutcome {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompactionTrigger {
    BeforeTurn,
    BetweenModelRequests,
    SmallerModel,
    ContextLengthExceeded,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CompactionCheckpoint {
    pub id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_leaf_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub trigger: CompactionTrigger,
    pub covered_from_message_index: usize,
    pub covered_through_message_index: usize,
    pub recent_tail_starts_at_message_index: usize,
    pub summary: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    #[serde(default)]
    pub usage: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_checkpoint_id: Option<Uuid>,
}

impl CompactionCheckpoint {
    #[cfg(test)]
    pub(crate) fn test(recent_tail_starts_at_message_index: usize, summary: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            branch_leaf_id: None,
            created_at: Utc::now(),
            trigger: CompactionTrigger::BeforeTurn,
            covered_from_message_index: 0,
            covered_through_message_index: recent_tail_starts_at_message_index.saturating_sub(1),
            recent_tail_starts_at_message_index,
            summary: summary.into(),
            provider: "test".into(),
            model: "test".into(),
            reasoning_effort: None,
            service_tier: None,
            usage: TokenUsage::default(),
            previous_checkpoint_id: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RequestUsage {
    pub recorded_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_leaf_id: Option<Uuid>,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub canonical_message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<Uuid>,
    pub usage: TokenUsage,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ScheduledRun {
    pub job_id: String,
    pub occurrence: u64,
    pub scheduled_at: DateTime<Utc>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Session {
    pub id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_run: Option<ScheduledRun>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default = "default_provider")]
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub title: String,
    #[serde(rename = "conversation")]
    pub messages: ConversationTree,
    pub activities: Vec<AgentActivity>,
    #[serde(default)]
    next_event_sequence: u64,
    #[serde(default)]
    pub turns: Vec<AgentTurn>,
    #[serde(default)]
    pub goals: Vec<Goal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_goal_id: Option<Uuid>,
    #[serde(default)]
    pub compaction_checkpoints: Vec<CompactionCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_request_usage: Option<RequestUsage>,
    #[serde(default = "default_next_terminal_id")]
    pub next_terminal_id: u64,
    #[serde(default)]
    pub terminals: Vec<TerminalRecord>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub file_changes: Vec<FileChangeSet>,
}

fn default_provider() -> String {
    crate::config::DEFAULT_PROVIDER.into()
}

fn default_next_terminal_id() -> u64 {
    1
}

impl Session {
    pub(crate) fn reserve_event_sequence(&mut self) -> u64 {
        let sequence = self.next_event_sequence;
        self.next_event_sequence = self.next_event_sequence.saturating_add(1);
        sequence
    }

    pub(crate) fn start_turn(
        &mut self,
        message_id: Uuid,
        message_index: usize,
        started_at: DateTime<Utc>,
    ) {
        self.turns.push(AgentTurn {
            message_id,
            message_index,
            started_at,
            completed_at: None,
            outcome: None,
            change_id: None,
        });
    }

    pub(crate) fn complete_turn(&mut self, message_id: Uuid, completed_at: DateTime<Utc>) {
        if let Some(turn) = self
            .turns
            .iter_mut()
            .rev()
            .find(|turn| turn.message_id == message_id)
        {
            turn.completed_at = Some(completed_at);
            turn.outcome = Some(TurnOutcome::Completed);
        }
    }

    pub(crate) fn finish_latest_turn(&mut self, outcome: TurnOutcome, completed_at: DateTime<Utc>) {
        if let Some(turn) = self.turns.last_mut() {
            turn.completed_at = Some(completed_at);
            turn.outcome = Some(outcome);
        }
        self.updated_at = completed_at;
    }

    pub(crate) fn active_goal(&self) -> Option<&Goal> {
        self.goals
            .iter()
            .find(|goal| goal.status == GoalStatus::Active)
    }

    pub(crate) fn create_goal(&mut self, objective: String) -> Uuid {
        self.pause_active_goal();
        let now = Utc::now();
        let id = Uuid::new_v4();
        self.goals.push(Goal {
            id,
            objective,
            status: GoalStatus::Active,
            created_at: now,
            updated_at: now,
            status_detail: None,
        });
        self.visible_goal_id = Some(id);
        self.updated_at = now;
        id
    }

    pub(crate) fn edit_goal(&mut self, id: Uuid, objective: String) -> bool {
        let Some(goal) = self.goals.iter_mut().find(|goal| goal.id == id) else {
            return false;
        };
        goal.objective = objective;
        if matches!(goal.status, GoalStatus::Completed | GoalStatus::Blocked) {
            goal.status = GoalStatus::Paused;
        }
        goal.updated_at = Utc::now();
        goal.status_detail = None;
        self.visible_goal_id = Some(id);
        self.updated_at = Utc::now();
        true
    }

    pub(crate) fn activate_goal(&mut self, id: Uuid) -> bool {
        if !self.goals.iter().any(|goal| goal.id == id) {
            return false;
        }
        self.pause_active_goal();
        let now = Utc::now();
        let goal = self
            .goals
            .iter_mut()
            .find(|goal| goal.id == id)
            .expect("goal existence was checked");
        goal.status = GoalStatus::Active;
        goal.updated_at = now;
        goal.status_detail = None;
        self.visible_goal_id = Some(id);
        self.updated_at = now;
        true
    }

    pub(crate) fn pause_goal(&mut self, id: Uuid) -> bool {
        let Some(goal) = self.goals.iter_mut().find(|goal| goal.id == id) else {
            return false;
        };
        goal.status = GoalStatus::Paused;
        goal.updated_at = Utc::now();
        goal.status_detail = None;
        self.visible_goal_id = Some(id);
        self.updated_at = Utc::now();
        true
    }

    pub(crate) fn pause_active_goal(&mut self) -> Option<Uuid> {
        let goal = self
            .goals
            .iter_mut()
            .find(|goal| goal.status == GoalStatus::Active)?;
        goal.status = GoalStatus::Paused;
        goal.updated_at = Utc::now();
        self.updated_at = Utc::now();
        Some(goal.id)
    }

    pub(crate) fn finish_active_goal(
        &mut self,
        status: GoalStatus,
        detail: Option<String>,
    ) -> Option<Uuid> {
        debug_assert!(matches!(
            status,
            GoalStatus::Completed | GoalStatus::Blocked
        ));
        let goal = self
            .goals
            .iter_mut()
            .find(|goal| goal.status == GoalStatus::Active)?;
        goal.status = status;
        goal.updated_at = Utc::now();
        goal.status_detail = detail;
        self.visible_goal_id = Some(goal.id);
        self.updated_at = Utc::now();
        Some(goal.id)
    }

    pub(crate) fn delete_goal(&mut self, id: Uuid) -> bool {
        let Some(index) = self.goals.iter().position(|goal| goal.id == id) else {
            return false;
        };
        self.goals.remove(index);
        if self.visible_goal_id == Some(id) {
            self.visible_goal_id = self
                .goals
                .iter()
                .max_by_key(|goal| goal.updated_at)
                .map(|goal| goal.id);
        }
        self.updated_at = Utc::now();
        true
    }

    pub(crate) fn preview_branch(&self, node_id: Uuid) -> Result<Self> {
        let mut preview = self.clone();
        preview.select_branch(node_id)?;
        Ok(preview)
    }

    pub(crate) fn select_branch(&mut self, node_id: Uuid) -> Result<Uuid> {
        let leaf_id = self.messages.select_from_node(node_id)?;
        self.refresh_active_indexes();
        self.updated_at = Utc::now();
        Ok(leaf_id)
    }

    #[cfg(test)]
    pub(crate) fn edit_user_message(&mut self, node_id: Uuid, content: String) -> Result<Uuid> {
        self.edit_user_message_with(node_id, Message::text(Role::User, content))
    }

    pub(crate) fn edit_user_message_with(
        &mut self,
        node_id: Uuid,
        message: Message,
    ) -> Result<Uuid> {
        let content = message.content.clone().unwrap_or_default();
        if content.trim().is_empty() {
            anyhow::bail!("edited message is empty");
        }
        let editing_root = self.messages.message(node_id).is_some_and(|_| {
            self.messages
                .nodes
                .iter()
                .find(|node| node.id == node_id)
                .is_some_and(|node| node.parent_id.is_none())
        });
        let edited_id = self
            .messages
            .branch_from_edited_user_message(node_id, message)?;
        self.refresh_active_indexes();
        if editing_root {
            self.title = content.chars().take(72).collect();
        }
        self.updated_at = Utc::now();
        Ok(edited_id)
    }

    pub(crate) fn latest_compaction(&self) -> Option<&CompactionCheckpoint> {
        let active_leaf_id = self.messages.active_leaf_id();
        self.compaction_checkpoints.iter().rev().find(|checkpoint| {
            checkpoint.branch_leaf_id.is_none_or(|checkpoint_leaf_id| {
                active_leaf_id.is_some_and(|active_leaf_id| {
                    self.messages
                        .is_ancestor(checkpoint_leaf_id, active_leaf_id)
                })
            })
        })
    }

    pub(crate) fn latest_request_usage(&self) -> Option<&RequestUsage> {
        let usage = self.latest_request_usage.as_ref()?;
        let active_leaf_id = self.messages.active_leaf_id();
        usage
            .branch_leaf_id
            .is_none_or(|usage_leaf_id| {
                active_leaf_id.is_some_and(|active_leaf_id| {
                    self.messages.is_ancestor(usage_leaf_id, active_leaf_id)
                })
            })
            .then_some(usage)
    }

    fn refresh_active_indexes(&mut self) {
        let indexes = self
            .messages
            .active_node_ids()
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, index))
            .collect::<HashMap<_, _>>();
        for activity in &mut self.activities {
            activity.turn_message_index = indexes
                .get(&activity.turn_message_id)
                .copied()
                .unwrap_or(usize::MAX);
        }
        for turn in &mut self.turns {
            turn.message_index = indexes.get(&turn.message_id).copied().unwrap_or(usize::MAX);
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct SessionSummary {
    pub id: Uuid,
    pub parent_session_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled_run: Option<ScheduledRun>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub title: String,
    pub model: String,
    pub depth: usize,
    pub descendant_count: usize,
}

#[derive(Clone, Serialize)]
pub(crate) struct SessionProject {
    pub root: PathBuf,
    pub sessions: Vec<SessionSummary>,
}

#[derive(Clone)]
pub(crate) struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub(crate) fn new(root: &Path) -> Result<Self> {
        let root = normalized_root(root);
        let dir = root.join(".codecrab").join("sessions");
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        Ok(Self { dir })
    }

    #[cfg(test)]
    pub(crate) fn create(&self, model: String) -> Result<Session> {
        self.create_for_provider(default_provider(), model)
    }

    pub(crate) fn create_for_provider(&self, provider: String, model: String) -> Result<Session> {
        let now = Utc::now();
        Ok(Session {
            id: Uuid::new_v4(),
            parent_session_id: None,
            scheduled_run: None,
            created_at: now,
            updated_at: now,
            provider,
            model,
            reasoning_effort: None,
            service_tier: None,
            title: "New session".into(),
            messages: ConversationTree::default(),
            activities: Vec::new(),
            next_event_sequence: 0,
            turns: Vec::new(),
            goals: Vec::new(),
            visible_goal_id: None,
            compaction_checkpoints: Vec::new(),
            latest_request_usage: None,
            next_terminal_id: 1,
            terminals: Vec::new(),
            attachments: Vec::new(),
            file_changes: Vec::new(),
        })
    }

    pub(crate) fn save(&self, session: &Session) -> Result<()> {
        let path = self.dir.join(format!("{}.json", session.id));
        Self::write(&path, session)
    }

    fn write(path: &Path, session: &Session) -> Result<()> {
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(session)?)
            .with_context(|| format!("cannot write {}", temp.display()))?;
        // Windows does not replace an existing destination with rename.
        if path.exists() {
            fs::remove_file(path).with_context(|| format!("cannot replace {}", path.display()))?;
        }
        fs::rename(&temp, path).with_context(|| format!("cannot save {}", path.display()))?;
        Ok(())
    }

    pub(crate) fn list(&self) -> Result<Vec<SessionSummary>> {
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let session = self.read(&path)?;
            sessions.push(SessionSummary {
                id: session.id,
                parent_session_id: session.parent_session_id,
                scheduled_run: session.scheduled_run.clone(),
                created_at: session.created_at,
                updated_at: session.updated_at,
                title: session.title,
                model: session.model,
                depth: 0,
                descendant_count: 0,
            });
        }
        sessions.sort_by_key(|session| std::cmp::Reverse((session.created_at, session.id)));
        Ok(sessions)
    }

    pub(crate) fn load(&self, query: Option<&str>) -> Result<Session> {
        let id = self.resolve_id(query)?;
        self.read(&self.dir.join(format!("{id}.json")))
    }

    pub(crate) fn delete(&self, query: &str) -> Result<Uuid> {
        let id = self.resolve_id(Some(query))?;
        let path = self.dir.join(format!("{id}.json"));
        let project_root = self
            .dir
            .parent()
            .and_then(Path::parent)
            .context("session store has no project root")?;
        let attachment_store = AttachmentStore::new(project_root);
        let attachment_dir = attachment_store.session_dir(id);
        let tombstone = attachment_dir.with_extension(format!("deleting-{}", Uuid::new_v4()));
        let moved_data = if attachment_dir.exists() {
            fs::rename(&attachment_dir, &tombstone).with_context(|| {
                format!(
                    "cannot prepare attachment data {} for deletion",
                    attachment_dir.display()
                )
            })?;
            true
        } else {
            false
        };
        if let Err(error) = fs::remove_file(&path) {
            if moved_data {
                let _ = fs::rename(&tombstone, &attachment_dir);
            }
            return Err(error).with_context(|| format!("cannot delete {}", path.display()));
        }
        if moved_data {
            fs::remove_dir_all(&tombstone).with_context(|| {
                format!(
                    "session was deleted, but cannot remove {}",
                    tombstone.display()
                )
            })?;
        }
        Ok(id)
    }

    fn resolve_id(&self, query: Option<&str>) -> Result<Uuid> {
        let sessions = self.list()?;
        Ok(match query {
            None => {
                sessions
                    .iter()
                    .max_by_key(|session| (session.updated_at, session.id))
                    .context("there are no saved sessions")?
                    .id
            }
            Some(prefix) => {
                let matches: Vec<_> = sessions
                    .iter()
                    .filter(|s| s.id.to_string().starts_with(prefix))
                    .collect();
                match matches.as_slice() {
                    [one] => one.id,
                    [] => anyhow::bail!("no session matches {prefix:?}"),
                    _ => anyhow::bail!("session prefix {prefix:?} is ambiguous"),
                }
            }
        })
    }

    fn read(&self, path: &Path) -> Result<Session> {
        let bytes = fs::read(path)?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid session {}", path.display()))?;
        let missing_created_at = value.get("created_at").is_none();
        if missing_created_at {
            let updated_at = value
                .get("updated_at")
                .cloned()
                .context("session is missing updated_at")?;
            value
                .as_object_mut()
                .context("session JSON is not an object")?
                .insert("created_at".into(), updated_at);
        }
        let mut session: Session = serde_json::from_value(value)
            .with_context(|| format!("invalid session {}", path.display()))?;
        session.refresh_active_indexes();
        if missing_created_at {
            Self::write(path, &session)
                .with_context(|| format!("cannot add created_at to {}", path.display()))?;
        }
        Ok(session)
    }
}

pub(crate) fn list_session_projects(
    current_root: &Path,
    registry: &SessionRegistry,
) -> Result<Vec<SessionProject>> {
    let current_root = normalized_root(current_root);
    let mut roots = vec![current_root.clone()];
    for root in registry.directories()? {
        let root = normalized_root(&root);
        if !roots.iter().any(|existing| paths_equal(existing, &root)) {
            roots.push(root);
        }
    }

    let mut projects = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let store = SessionStore::new(&root)?;
        let sessions = store.list()?;
        projects.push(SessionProject { root, sessions });
    }
    arrange_session_projects(&mut projects);
    Ok(projects)
}

pub(crate) fn arrange_session_projects(projects: &mut [SessionProject]) {
    for project in projects {
        arrange_session_tree(&mut project.sessions);
    }
}

fn arrange_session_tree(sessions: &mut Vec<SessionSummary>) {
    let source = std::mem::take(sessions);
    let indexes = source
        .iter()
        .enumerate()
        .map(|(index, session)| (session.id, index))
        .collect::<HashMap<_, _>>();
    let mut cyclic = HashSet::new();

    for start in 0..source.len() {
        let mut positions = HashMap::new();
        let mut path = Vec::new();
        let mut current = start;
        loop {
            if let Some(cycle_start) = positions.insert(current, path.len()) {
                cyclic.extend(path[cycle_start..].iter().copied());
                break;
            }
            path.push(current);
            let Some(parent) = source[current]
                .parent_session_id
                .and_then(|id| indexes.get(&id))
                .copied()
            else {
                break;
            };
            current = parent;
        }
    }

    let mut roots = Vec::new();
    let mut children = vec![Vec::new(); source.len()];
    for (index, session) in source.iter().enumerate() {
        let parent = session
            .parent_session_id
            .and_then(|id| indexes.get(&id))
            .copied();
        if cyclic.contains(&index) || parent.is_none() {
            roots.push(index);
        } else if let Some(parent) = parent {
            children[parent].push(index);
        }
    }

    let newest_first = |left: &usize, right: &usize| {
        (source[*right].created_at, source[*right].id)
            .cmp(&(source[*left].created_at, source[*left].id))
    };
    roots.sort_by(newest_first);
    for siblings in &mut children {
        siblings.sort_by(newest_first);
    }

    fn descendant_count(index: usize, children: &[Vec<usize>]) -> usize {
        children[index]
            .iter()
            .map(|child| 1 + descendant_count(*child, children))
            .sum()
    }

    fn append_tree(
        index: usize,
        depth: usize,
        source: &[SessionSummary],
        children: &[Vec<usize>],
        output: &mut Vec<SessionSummary>,
    ) {
        let mut session = source[index].clone();
        session.depth = depth;
        session.descendant_count = descendant_count(index, children);
        output.push(session);
        for child in &children[index] {
            append_tree(*child, depth + 1, source, children, output);
        }
    }

    sessions.reserve(source.len());
    for root in roots {
        append_tree(root, 0, &source, &children, sessions);
    }
}

pub(crate) fn resolve_global_session(
    projects: &[SessionProject],
    query: Option<&str>,
) -> Result<(PathBuf, Uuid)> {
    let mut sessions = projects
        .iter()
        .flat_map(|project| {
            project
                .sessions
                .iter()
                .map(move |session| (&project.root, session))
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|(_, session)| std::cmp::Reverse((session.updated_at, session.id)));
    match query {
        None => sessions
            .first()
            .map(|(root, session)| ((*root).clone(), session.id))
            .context("there are no saved sessions"),
        Some(prefix) => {
            let matches = sessions
                .into_iter()
                .filter(|(_, session)| session.id.to_string().starts_with(prefix))
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [(root, session)] => Ok(((*root).clone(), session.id)),
                [] => anyhow::bail!("no session matches {prefix:?}"),
                _ => anyhow::bail!("session prefix {prefix:?} is ambiguous"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ActivityKind, ActivityStatus, AgentActivity};

    #[test]
    fn conversation_tree_preserves_branches_and_projects_only_the_active_path() {
        let mut tree = ConversationTree::default();
        let root = tree.push(Message::text(crate::provider::Role::User, "root"));
        let original = tree.push(Message::text(crate::provider::Role::Assistant, "original"));
        let original_leaf = tree.push(Message::text(crate::provider::Role::User, "original leaf"));

        let alternate = tree
            .branch_from(
                Some(root),
                Message::text(crate::provider::Role::Assistant, "alternate"),
            )
            .unwrap();

        assert_eq!(tree.nodes.len(), 4);
        assert_eq!(tree.active_leaf_id(), Some(alternate));
        assert_eq!(tree.active_node_ids(), &[root, alternate]);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].content.as_deref(), Some("root"));
        assert_eq!(tree[1].content.as_deref(), Some("alternate"));
        assert!(tree.is_ancestor(root, alternate));
        assert!(!tree.is_ancestor(original, alternate));
        assert_eq!(
            tree.nodes
                .iter()
                .find(|node| node.id == original_leaf)
                .and_then(|node| node.message.content.as_deref()),
            Some("original leaf")
        );
    }

    #[test]
    fn selecting_an_intermediate_node_follows_its_oldest_descendant_to_a_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        let root = session
            .messages
            .push(Message::text(crate::provider::Role::User, "root"));
        let original_answer = session.messages.push(Message::text(
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
                Some(root),
                Message::text(crate::provider::Role::Assistant, "newer answer"),
            )
            .unwrap();
        session.messages.push(Message::text(
            crate::provider::Role::User,
            "newer follow-up",
        ));

        let selected_leaf = session.select_branch(root).unwrap();

        assert_eq!(selected_leaf, original_leaf);
        assert_eq!(
            session.messages.active_node_ids(),
            &[root, original_answer, original_leaf]
        );
        assert_eq!(
            session.messages[2].content.as_deref(),
            Some("original follow-up")
        );
    }

    #[test]
    fn visible_conversation_graph_connects_user_turns_across_agent_messages() {
        let mut tree = ConversationTree::default();
        let root = tree.push(Message::text(crate::provider::Role::User, "root"));
        let answer = tree.push(Message::text(crate::provider::Role::Assistant, "answer"));
        let follow_up = tree.push(Message::text(crate::provider::Role::User, "follow up"));
        let alternate = tree
            .branch_from(
                Some(answer),
                Message::text(crate::provider::Role::User, "alternate follow up"),
            )
            .unwrap();

        let graph = tree.visible_user_nodes();

        assert_eq!(graph.len(), 3);
        assert_eq!(graph[0].id, root);
        assert_eq!(graph[0].parent_id, None);
        assert_eq!(graph[1].id, follow_up);
        assert_eq!(graph[1].parent_id, Some(root));
        assert_eq!(graph[2].id, alternate);
        assert_eq!(graph[2].parent_id, Some(root));
    }

    #[test]
    fn editing_a_user_message_creates_a_new_branch_and_preserves_the_original_continuation() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        let root = session
            .messages
            .push(Message::text(crate::provider::Role::User, "root"));
        let answer = session.messages.push(Message::text(
            crate::provider::Role::Assistant,
            "original answer",
        ));
        let original_message = session.messages.push(Message::text(
            crate::provider::Role::User,
            "original follow-up",
        ));
        let original_leaf = session.messages.push(Message::text(
            crate::provider::Role::Assistant,
            "original continuation",
        ));

        let edited_message = session
            .edit_user_message(original_message, "edited follow-up".into())
            .unwrap();

        assert_eq!(
            session.messages.active_node_ids(),
            &[root, answer, edited_message]
        );
        assert_eq!(
            session
                .messages
                .last()
                .and_then(|message| message.content.as_deref()),
            Some("edited follow-up")
        );
        assert_eq!(session.messages.nodes.len(), 5);
        assert_eq!(
            session
                .messages
                .message(original_leaf)
                .and_then(|message| message.content.as_deref()),
            Some("original continuation")
        );
    }

    #[test]
    fn editing_the_first_message_updates_the_session_title() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        let root = session
            .messages
            .push(Message::text(crate::provider::Role::User, "old title"));

        session
            .edit_user_message(root, "A clearer edited request".into())
            .unwrap();

        assert_eq!(session.title, "A clearer edited request");
    }

    #[test]
    fn persisted_sessions_store_the_tree_without_a_duplicate_linear_history() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        let root = session
            .messages
            .push(Message::text(crate::provider::Role::User, "root"));
        session
            .messages
            .push(Message::text(crate::provider::Role::Assistant, "original"));
        let alternate = session
            .messages
            .branch_from(
                Some(root),
                Message::text(crate::provider::Role::Assistant, "alternate"),
            )
            .unwrap();
        store.save(&session).unwrap();

        let path = temp
            .path()
            .join(".codecrab")
            .join("sessions")
            .join(format!("{}.json", session.id));
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(persisted.get("messages").is_none());
        assert_eq!(
            persisted["conversation"]["nodes"].as_array().unwrap().len(),
            3
        );
        assert_eq!(
            persisted["conversation"]["active_leaf_id"],
            serde_json::to_value(alternate).unwrap()
        );

        let loaded = store.load(Some(&session.id.to_string())).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].content.as_deref(), Some("root"));
        assert_eq!(loaded.messages[1].content.as_deref(), Some("alternate"));
        assert_eq!(loaded.messages.nodes.len(), 3);
    }

    #[test]
    fn deserialization_rejects_cycles_in_inactive_branches() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let active = Uuid::new_v4();
        let value = serde_json::json!({
            "nodes": [
                {
                    "id": first,
                    "parent_id": second,
                    "message": {"role": "user", "content": "first"}
                },
                {
                    "id": second,
                    "parent_id": first,
                    "message": {"role": "assistant", "content": "second"}
                },
                {
                    "id": active,
                    "message": {"role": "user", "content": "active"}
                }
            ],
            "active_leaf_id": active
        });

        let error = serde_json::from_value::<ConversationTree>(value)
            .err()
            .expect("an inactive cycle must make the whole tree invalid");
        assert!(error.to_string().contains("contains a cycle"));
    }

    #[test]
    fn branch_specific_metadata_is_not_reused_on_an_alternate_path() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        let root = session
            .messages
            .push(Message::text(crate::provider::Role::User, "root"));
        let original_turn = session.messages.push(Message::text(
            crate::provider::Role::User,
            "original follow-up",
        ));
        let mut checkpoint = CompactionCheckpoint::test(1, "original branch summary");
        checkpoint.branch_leaf_id = Some(original_turn);
        session.compaction_checkpoints.push(checkpoint);
        session.latest_request_usage = Some(RequestUsage {
            recorded_at: Utc::now(),
            branch_leaf_id: Some(original_turn),
            provider: session.provider.clone(),
            model: session.model.clone(),
            reasoning_effort: None,
            service_tier: None,
            canonical_message_count: session.messages.len(),
            checkpoint_id: None,
            usage: TokenUsage::default(),
        });
        session.activities.push(AgentActivity {
            id: "original-activity".into(),
            turn_message_id: original_turn,
            turn_message_index: 1,
            sequence: None,
            started_at: None,
            completed_at: None,
            tool: "read_file".into(),
            kind: ActivityKind::Read,
            status: ActivityStatus::Completed,
            title: "Read".into(),
            detail: "original.txt".into(),
            change_id: None,
            live_change_id: None,
        });
        session.start_turn(original_turn, 1, Utc::now());

        session
            .messages
            .branch_from(
                Some(root),
                Message::text(crate::provider::Role::User, "alternate follow-up"),
            )
            .unwrap();
        assert!(session.latest_compaction().is_none());
        assert!(session.latest_request_usage().is_none());

        store.save(&session).unwrap();
        let loaded = store.load(Some(&session.id.to_string())).unwrap();
        assert_eq!(loaded.activities[0].turn_message_index, usize::MAX);
        assert_eq!(loaded.turns[0].message_index, usize::MAX);
        assert!(loaded.latest_compaction().is_none());
        assert!(loaded.latest_request_usage().is_none());
    }

    #[test]
    fn provider_is_persisted_with_the_session() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let session = store
            .create_for_provider("example".into(), "example-model".into())
            .unwrap();
        store.save(&session).unwrap();

        let loaded = store.load(Some(&session.id.to_string())).unwrap();
        assert_eq!(loaded.provider, "example");
        assert_eq!(loaded.model, "example-model");
    }

    #[test]
    fn scheduled_run_lineage_is_persisted_and_exposed_in_session_lists() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        session.scheduled_run = Some(ScheduledRun {
            job_id: "nightly".into(),
            occurrence: 7,
            scheduled_at: Utc::now(),
        });
        store.save(&session).unwrap();

        let loaded = store.load(Some(&session.id.to_string())).unwrap();
        assert_eq!(loaded.scheduled_run.as_ref().unwrap().job_id, "nightly");
        let summary = store.list().unwrap().pop().unwrap();
        assert_eq!(summary.scheduled_run.unwrap().occurrence, 7);
    }

    #[test]
    fn created_at_is_persisted_and_does_not_follow_updates() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        let created_at = session.created_at;
        session.updated_at = created_at + chrono::Duration::hours(1);
        store.save(&session).unwrap();

        let loaded = store.load(Some(&session.id.to_string())).unwrap();
        assert_eq!(loaded.created_at, created_at);
        assert_eq!(loaded.updated_at, session.updated_at);
    }

    #[test]
    fn loading_a_legacy_session_persists_updated_at_as_created_at() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        session.updated_at += chrono::Duration::hours(2);
        store.save(&session).unwrap();
        let path = store.dir.join(format!("{}.json", session.id));
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("created_at");
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let loaded = store.load(Some(&session.id.to_string())).unwrap();
        assert_eq!(loaded.created_at, session.updated_at);
        let migrated: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(migrated["created_at"], migrated["updated_at"]);
    }

    #[test]
    fn lists_by_creation_but_default_resume_uses_latest_update() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let now = Utc::now();
        let mut older = store.create("older".into()).unwrap();
        older.created_at = now - chrono::Duration::hours(2);
        older.updated_at = now;
        let mut newer = store.create("newer".into()).unwrap();
        newer.created_at = now - chrono::Duration::hours(1);
        newer.updated_at = now - chrono::Duration::minutes(30);
        store.save(&older).unwrap();
        store.save(&newer).unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![newer.id, older.id]
        );
        assert_eq!(store.load(None).unwrap().id, older.id);
        let projects = vec![SessionProject {
            root: temp.path().to_path_buf(),
            sessions,
        }];
        assert_eq!(resolve_global_session(&projects, None).unwrap().1, older.id);
    }

    #[test]
    fn terminal_records_and_monotonic_counter_are_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        let now = Utc::now();
        session.next_terminal_id = 4;
        session.terminals.push(TerminalRecord {
            id: "terminal_3".into(),
            command: "interactive-command".into(),
            shell: "/bin/sh".into(),
            working_directory: temp.path().to_path_buf(),
            created_at: now,
            updated_at: now,
            completed_at: None,
            columns: 120,
            rows: 40,
            state: crate::terminal::TerminalProcessState::Running,
            exit_code: None,
            latest_snapshot: None,
            latest_observation: crate::terminal::ObservationClassification::Unchanged,
            recent_transcript: "prompt".into(),
        });

        store.save(&session).unwrap();
        let loaded = store.load(Some(&session.id.to_string())).unwrap();

        assert_eq!(loaded.next_terminal_id, 4);
        assert_eq!(loaded.terminals.len(), 1);
        assert_eq!(loaded.terminals[0].id, "terminal_3");
        assert_eq!(
            loaded.terminals[0].state,
            crate::terminal::TerminalProcessState::Running
        );
        assert_eq!(loaded.terminals[0].recent_transcript, "prompt");
    }

    #[test]
    fn compaction_checkpoints_reload_without_rewriting_raw_history() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        session.messages.push(Message::text(
            crate::provider::Role::User,
            "original request",
        ));
        session.messages.push(Message::text(
            crate::provider::Role::Assistant,
            "original exact answer",
        ));
        let checkpoint = CompactionCheckpoint::test(1, "rolling summary");
        let checkpoint_id = checkpoint.id;
        session.compaction_checkpoints.push(checkpoint);
        session.latest_request_usage = Some(RequestUsage {
            recorded_at: Utc::now(),
            branch_leaf_id: None,
            provider: session.provider.clone(),
            model: session.model.clone(),
            reasoning_effort: None,
            service_tier: None,
            canonical_message_count: session.messages.len(),
            checkpoint_id: Some(checkpoint_id),
            usage: TokenUsage {
                input_tokens: Some(42),
                output_tokens: Some(7),
                ..TokenUsage::default()
            },
        });
        store.save(&session).unwrap();

        let loaded = store.load(Some(&session.id.to_string())).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(
            loaded.messages[1].content.as_deref(),
            Some("original exact answer")
        );
        assert_eq!(loaded.compaction_checkpoints.len(), 1);
        assert_eq!(loaded.compaction_checkpoints[0].id, checkpoint_id);
        assert_eq!(
            loaded
                .latest_request_usage
                .as_ref()
                .and_then(|usage| usage.usage.input_tokens),
            Some(42)
        );
    }

    #[test]
    fn event_timestamps_are_persisted_with_the_session() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        let started_at = Utc::now();
        let completed_at = started_at + chrono::Duration::seconds(7);
        let mut message = Message::text(crate::provider::Role::User, "Inspect timestamps");
        message.created_at = Some(started_at);
        let message_id = session.messages.push(message);
        session.activities.push(AgentActivity {
            id: "call-timestamp".into(),
            turn_message_id: message_id,
            turn_message_index: 0,
            sequence: Some(1),
            started_at: Some(started_at),
            completed_at: Some(completed_at),
            tool: "read_file".into(),
            kind: ActivityKind::Read,
            status: ActivityStatus::Completed,
            title: "Read".into(),
            detail: "src/main.rs".into(),
            change_id: None,
            live_change_id: None,
        });
        session.start_turn(message_id, 0, started_at);
        session.complete_turn(message_id, completed_at);
        store.save(&session).unwrap();

        let loaded = store.load(Some(&session.id.to_string())).unwrap();
        assert_eq!(loaded.messages[0].created_at, Some(started_at));
        assert_eq!(loaded.activities[0].started_at, Some(started_at));
        assert_eq!(loaded.activities[0].completed_at, Some(completed_at));
        assert_eq!(loaded.turns.len(), 1);
        assert_eq!(loaded.turns[0].message_index, 0);
        assert_eq!(loaded.turns[0].started_at, started_at);
        assert_eq!(loaded.turns[0].completed_at, Some(completed_at));
    }

    #[test]
    fn event_sequence_continues_after_resuming_a_session() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        assert_eq!(session.reserve_event_sequence(), 0);
        assert_eq!(session.reserve_event_sequence(), 1);
        store.save(&session).unwrap();

        let mut loaded = store.load(Some(&session.id.to_string())).unwrap();
        assert_eq!(loaded.reserve_event_sequence(), 2);
    }

    #[test]
    fn a_session_can_be_saved_more_than_once() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        session.activities.push(AgentActivity {
            id: "call-1".into(),
            turn_message_id: Uuid::nil(),
            turn_message_index: 0,
            sequence: None,
            started_at: None,
            completed_at: None,
            tool: "read_file".into(),
            kind: ActivityKind::Read,
            status: ActivityStatus::Completed,
            title: "Read".into(),
            detail: "src/main.rs".into(),
            change_id: None,
            live_change_id: None,
        });
        store.save(&session).unwrap();
        session.title = "Updated".into();
        store.save(&session).unwrap();
        let loaded = store.load(Some(&session.id.to_string()[..8])).unwrap();
        assert_eq!(loaded.title, "Updated");
        assert_eq!(loaded.activities.len(), 1);
        assert_eq!(loaded.activities[0].detail, "src/main.rs");
    }

    #[test]
    fn a_session_can_be_deleted_by_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let session = store.create("test-model".into()).unwrap();
        store.save(&session).unwrap();

        let deleted = store.delete(&session.id.to_string()[..8]).unwrap();

        assert_eq!(deleted, session.id);
        assert!(store.list().unwrap().is_empty());
        assert!(store.load(Some(&session.id.to_string())).is_err());
    }

    #[test]
    fn deleting_a_session_removes_its_attachment_directory() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path()).unwrap();
        let session = store.create("model".into()).unwrap();
        store.save(&session).unwrap();
        let attachment_dir = AttachmentStore::new(root.path()).session_dir(session.id);
        fs::create_dir_all(attachment_dir.join("attachments/hash")).unwrap();
        fs::write(attachment_dir.join("attachments/hash/original"), b"data").unwrap();

        store.delete(&session.id.to_string()).unwrap();

        assert!(!attachment_dir.exists());
        assert!(store.load(Some(&session.id.to_string())).is_err());
    }

    #[test]
    fn deleting_a_parent_preserves_child_lineage_and_promotes_it_to_a_root() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path()).unwrap();
        let parent = store.create("model".into()).unwrap();
        let mut child = store.create("model".into()).unwrap();
        child.parent_session_id = Some(parent.id);
        store.save(&parent).unwrap();
        store.save(&child).unwrap();

        store.delete(&parent.id.to_string()).unwrap();

        let persisted_child = store.load(Some(&child.id.to_string())).unwrap();
        assert_eq!(persisted_child.parent_session_id, Some(parent.id));
        let registry = SessionRegistry::at(root.path().join("config.toml"));
        let projects = list_session_projects(root.path(), &registry).unwrap();
        assert_eq!(projects[0].sessions.len(), 1);
        assert_eq!(projects[0].sessions[0].id, child.id);
        assert_eq!(projects[0].sessions[0].depth, 0);
    }

    #[test]
    fn goals_are_persisted_with_only_one_active_at_a_time() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();

        let first = session.create_goal("Finish the migration".into());
        let second = session.create_goal("Fix the release build".into());

        assert_eq!(
            session
                .goals
                .iter()
                .find(|goal| goal.id == first)
                .unwrap()
                .status,
            GoalStatus::Paused
        );
        assert_eq!(session.active_goal().unwrap().id, second);
        assert_eq!(session.visible_goal_id, Some(second));

        assert!(session.activate_goal(first));
        assert_eq!(session.active_goal().unwrap().id, first);
        assert_eq!(
            session
                .goals
                .iter()
                .find(|goal| goal.id == second)
                .unwrap()
                .status,
            GoalStatus::Paused
        );
        session.finish_active_goal(GoalStatus::Completed, Some("Tests pass".into()));
        store.save(&session).unwrap();

        let loaded = store.load(Some(&session.id.to_string())).unwrap();
        assert_eq!(loaded.goals.len(), 2);
        assert_eq!(loaded.visible_goal_id, Some(first));
        assert_eq!(
            loaded
                .goals
                .iter()
                .find(|goal| goal.id == first)
                .unwrap()
                .status,
            GoalStatus::Completed
        );
    }

    #[test]
    fn lists_current_project_first_and_resolves_sessions_globally() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("current");
        let other = temp.path().join("other");
        fs::create_dir_all(&current).unwrap();
        fs::create_dir_all(&other).unwrap();
        let registry = SessionRegistry::at(temp.path().join("config.toml"));
        registry.register(&other).unwrap();
        let current_store = SessionStore::new(&current).unwrap();
        let other_store = SessionStore::new(&other).unwrap();
        let current_session = current_store.create("current-model".into()).unwrap();
        let other_session = other_store.create("other-model".into()).unwrap();
        current_store.save(&current_session).unwrap();
        other_store.save(&other_session).unwrap();

        let projects = list_session_projects(&current, &registry).unwrap();

        assert!(paths_equal(&projects[0].root, &current));
        assert!(paths_equal(&projects[1].root, &other));
        let (root, id) =
            resolve_global_session(&projects, Some(&other_session.id.to_string()[..8])).unwrap();
        assert!(paths_equal(&root, &other));
        assert_eq!(id, other_session.id);
    }

    #[test]
    fn session_hierarchy_is_recursive_ordered_and_cycle_safe() {
        fn summary(id: u128, parent: Option<u128>, created_at: i64) -> SessionSummary {
            SessionSummary {
                id: Uuid::from_u128(id),
                parent_session_id: parent.map(Uuid::from_u128),
                scheduled_run: None,
                created_at: DateTime::from_timestamp(created_at, 0).unwrap(),
                updated_at: DateTime::from_timestamp(created_at, 0).unwrap(),
                title: format!("session-{id}"),
                model: "model".into(),
                depth: 99,
                descendant_count: 99,
            }
        }

        let mut projects = vec![
            SessionProject {
                root: PathBuf::from("first"),
                sessions: vec![
                    summary(1, None, 1),
                    summary(2, None, 10),
                    summary(3, Some(1), 5),
                    summary(4, Some(3), 6),
                    summary(5, Some(999), 9),
                    summary(6, Some(7), 7),
                    summary(7, Some(6), 4),
                ],
            },
            SessionProject {
                root: PathBuf::from("second"),
                sessions: vec![summary(8, Some(1), 8)],
            },
        ];

        arrange_session_projects(&mut projects);

        assert_eq!(
            projects[0]
                .sessions
                .iter()
                .map(|session| session.id.as_u128())
                .collect::<Vec<_>>(),
            [2, 5, 6, 7, 1, 3, 4]
        );
        let by_id = projects[0]
            .sessions
            .iter()
            .map(|session| (session.id.as_u128(), session))
            .collect::<HashMap<_, _>>();
        assert_eq!((by_id[&1].depth, by_id[&1].descendant_count), (0, 2));
        assert_eq!((by_id[&3].depth, by_id[&3].descendant_count), (1, 1));
        assert_eq!((by_id[&4].depth, by_id[&4].descendant_count), (2, 0));
        assert_eq!(by_id[&5].depth, 0, "missing parents become roots");
        assert_eq!(by_id[&6].depth, 0, "cycle members become roots");
        assert_eq!(by_id[&7].depth, 0, "cycle members become roots");
        assert_eq!(
            projects[1].sessions[0].depth, 0,
            "cross-project parents become roots"
        );
        assert_eq!(
            projects
                .iter()
                .map(|project| project.sessions.len())
                .sum::<usize>(),
            8
        );
        let projection = serde_json::to_value(&projects).unwrap();
        assert_eq!(
            projection[0]["sessions"][0]["parent_session_id"],
            serde_json::Value::Null
        );
        assert_eq!(
            projection[0]["sessions"][5]["parent_session_id"],
            Uuid::from_u128(1).to_string()
        );
        assert_eq!(projection[0]["sessions"][5]["depth"], 1);
        assert_eq!(projection[0]["sessions"][4]["descendant_count"], 2);
    }
}
