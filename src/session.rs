use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    config::{SessionRegistry, normalized_root, paths_equal},
    events::AgentActivity,
    provider::Message,
};

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

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Session {
    pub id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub title: String,
    pub messages: Vec<Message>,
    pub activities: Vec<AgentActivity>,
    #[serde(default)]
    pub goals: Vec<Goal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_goal_id: Option<Uuid>,
}

impl Session {
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
}

#[derive(Clone, Serialize)]
pub(crate) struct SessionSummary {
    pub id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub title: String,
    pub model: String,
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

    pub(crate) fn create(&self, model: String) -> Result<Session> {
        let now = Utc::now();
        Ok(Session {
            id: Uuid::new_v4(),
            updated_at: now,
            model,
            reasoning_effort: None,
            service_tier: None,
            title: "New session".into(),
            messages: Vec::new(),
            activities: Vec::new(),
            goals: Vec::new(),
            visible_goal_id: None,
        })
    }

    pub(crate) fn save(&self, session: &Session) -> Result<()> {
        let path = self.dir.join(format!("{}.json", session.id));
        let temp = self.dir.join(format!("{}.json.tmp", session.id));
        fs::write(&temp, serde_json::to_vec_pretty(session)?)
            .with_context(|| format!("cannot write {}", temp.display()))?;
        // Windows does not replace an existing destination with rename.
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("cannot replace {}", path.display()))?;
        }
        fs::rename(&temp, &path).with_context(|| format!("cannot save {}", path.display()))?;
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
                updated_at: session.updated_at,
                title: session.title,
                model: session.model,
            });
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(sessions)
    }

    pub(crate) fn load(&self, query: Option<&str>) -> Result<Session> {
        let id = self.resolve_id(query)?;
        self.read(&self.dir.join(format!("{id}.json")))
    }

    pub(crate) fn delete(&self, query: &str) -> Result<Uuid> {
        let id = self.resolve_id(Some(query))?;
        let path = self.dir.join(format!("{id}.json"));
        fs::remove_file(&path).with_context(|| format!("cannot delete {}", path.display()))?;
        Ok(id)
    }

    fn resolve_id(&self, query: Option<&str>) -> Result<Uuid> {
        let sessions = self.list()?;
        Ok(match query {
            None => sessions.first().context("there are no saved sessions")?.id,
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
        serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid session {}", path.display()))
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
        let dir = root.join(".codecrab").join("sessions");
        if !paths_equal(&root, &current_root) && !dir.is_dir() {
            continue;
        }
        let store = SessionStore::new(&root)?;
        let sessions = store.list()?;
        if paths_equal(&root, &current_root) || !sessions.is_empty() {
            projects.push(SessionProject { root, sessions });
        }
    }
    Ok(projects)
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
    sessions.sort_by_key(|(_, session)| std::cmp::Reverse(session.updated_at));
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
    fn a_session_can_be_saved_more_than_once() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        session.activities.push(AgentActivity {
            id: "call-1".into(),
            turn_message_index: 0,
            tool: "read_file".into(),
            kind: ActivityKind::Read,
            status: ActivityStatus::Completed,
            title: "Read".into(),
            detail: "src/main.rs".into(),
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
}
