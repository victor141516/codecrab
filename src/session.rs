use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::provider::Message;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Session {
    pub id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub title: String,
    pub messages: Vec<Message>,
}

pub(crate) struct SessionSummary {
    pub id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub title: String,
    pub model: String,
}

pub(crate) struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub(crate) fn new(root: &Path) -> Result<Self> {
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
        let sessions = self.list()?;
        let id = match query {
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
        };
        self.read(&self.dir.join(format!("{id}.json")))
    }

    fn read(&self, path: &Path) -> Result<Session> {
        let bytes = fs::read(path)?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid session {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_can_be_saved_more_than_once() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path()).unwrap();
        let mut session = store.create("test-model".into()).unwrap();
        store.save(&session).unwrap();
        session.title = "Updated".into();
        store.save(&session).unwrap();
        assert_eq!(
            store
                .load(Some(&session.id.to_string()[..8]))
                .unwrap()
                .title,
            "Updated"
        );
    }
}
