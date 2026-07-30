use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::provider::Message;

#[derive(Clone)]
pub(crate) enum AgentEvent {
    UserMessage(Message),
    AssistantMessage(Message),
    AssistantTextDelta {
        delta: String,
        start: bool,
        sequence: u64,
        created_at: DateTime<Utc>,
    },
    AssistantStreamReset,
    AssistantMessageCompleted(Message),
    Activity(AgentActivity),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActivityKind {
    Read,
    Search,
    Write,
    Shell,
    Skill,
    Network,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActivityStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct AgentActivity {
    pub id: String,
    pub turn_message_id: Uuid,
    pub turn_message_index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    pub tool: String,
    pub kind: ActivityKind,
    pub status: ActivityStatus,
    pub title: String,
    pub detail: String,
}

impl AgentActivity {
    pub(crate) fn started(
        id: String,
        turn_message_id: Uuid,
        turn_message_index: usize,
        sequence: u64,
        tool: &str,
        arguments: &str,
    ) -> Self {
        let (kind, running, _, _) = activity_labels(tool);
        Self {
            id,
            turn_message_id,
            turn_message_index,
            sequence: Some(sequence),
            started_at: Some(Utc::now()),
            completed_at: None,
            tool: tool.to_owned(),
            kind,
            status: ActivityStatus::Running,
            title: running.to_owned(),
            detail: activity_detail(tool, arguments),
        }
    }

    pub(crate) fn finish(&mut self, succeeded: bool) {
        let (_, _, completed, failed) = activity_labels(&self.tool);
        self.status = if succeeded {
            ActivityStatus::Completed
        } else {
            ActivityStatus::Failed
        };
        self.title = if succeeded { completed } else { failed }.to_owned();
        self.completed_at = Some(Utc::now());
    }

    pub(crate) fn model_retry(
        id: String,
        turn_message_id: Uuid,
        turn_message_index: usize,
        sequence: u64,
        retry: usize,
        max_retries: usize,
        error: String,
    ) -> Self {
        Self {
            id,
            turn_message_id,
            turn_message_index,
            sequence: Some(sequence),
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
            tool: "model_request".into(),
            kind: ActivityKind::Network,
            status: ActivityStatus::Completed,
            title: format!("Retrying model request ({retry}/{max_retries})"),
            detail: error,
        }
    }

    pub(crate) fn model_error(
        id: String,
        turn_message_id: Uuid,
        turn_message_index: usize,
        sequence: u64,
        error: String,
    ) -> Self {
        Self {
            id,
            turn_message_id,
            turn_message_index,
            sequence: Some(sequence),
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
            tool: "model_request".into(),
            kind: ActivityKind::Network,
            status: ActivityStatus::Failed,
            title: "Model request failed".into(),
            detail: error,
        }
    }

    pub(crate) fn compaction_started(
        id: String,
        turn_message_id: Uuid,
        turn_message_index: usize,
        sequence: u64,
        estimated_tokens: u64,
    ) -> Self {
        Self {
            id,
            turn_message_id,
            turn_message_index,
            sequence: Some(sequence),
            started_at: Some(Utc::now()),
            completed_at: None,
            tool: "context_compaction".into(),
            kind: ActivityKind::Other,
            status: ActivityStatus::Running,
            title: "Context compaction started".into(),
            detail: format!("Estimated active context: {estimated_tokens} tokens"),
        }
    }

    pub(crate) fn compaction_retry(
        id: String,
        turn_message_id: Uuid,
        turn_message_index: usize,
        sequence: u64,
        retry: usize,
        max_retries: usize,
        error: String,
    ) -> Self {
        Self {
            id,
            turn_message_id,
            turn_message_index,
            sequence: Some(sequence),
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
            tool: "context_compaction".into(),
            kind: ActivityKind::Network,
            status: ActivityStatus::Completed,
            title: format!("Retrying context compaction ({retry}/{max_retries})"),
            detail: error,
        }
    }

    pub(crate) fn finish_compaction(
        &mut self,
        succeeded: bool,
        before_tokens: u64,
        after_tokens: Option<u64>,
        error: Option<&str>,
    ) {
        self.status = if succeeded {
            ActivityStatus::Completed
        } else {
            ActivityStatus::Failed
        };
        self.completed_at = Some(Utc::now());
        if succeeded {
            self.title = "Context compacted".into();
            self.detail = format!(
                "{before_tokens} → {} tokens",
                after_tokens.unwrap_or_default()
            );
        } else {
            self.title = "Context compaction failed".into();
            self.detail = error
                .unwrap_or("the provider returned no usable summary")
                .into();
        }
    }
}

fn activity_labels(tool: &str) -> (ActivityKind, &'static str, &'static str, &'static str) {
    match tool {
        "list_files" => (ActivityKind::Read, "Listing", "Listed", "Failed to list"),
        "read_file" => (ActivityKind::Read, "Reading", "Read", "Failed to read"),
        "search" => (
            ActivityKind::Search,
            "Searching",
            "Searched",
            "Search failed",
        ),
        "write_file" => (ActivityKind::Write, "Writing", "Wrote", "Failed to write"),
        "replace_in_file" => (ActivityKind::Write, "Editing", "Edited", "Failed to edit"),
        "shell" | "shell_noninteractive" => (
            ActivityKind::Shell,
            "Running command",
            "Ran command",
            "Command failed",
        ),
        "terminal_input" => (
            ActivityKind::Shell,
            "Interacting with terminal",
            "Interacted with terminal",
            "Terminal interaction failed",
        ),
        "terminal_read" => (
            ActivityKind::Shell,
            "Observing terminal",
            "Observed terminal",
            "Terminal observation failed",
        ),
        "terminal_close" => (
            ActivityKind::Shell,
            "Closing terminal",
            "Closed terminal",
            "Failed to close terminal",
        ),
        "terminal_list" => (
            ActivityKind::Shell,
            "Listing terminals",
            "Listed terminals",
            "Failed to list terminals",
        ),
        "load_skill" => (
            ActivityKind::Skill,
            "Loading skill",
            "Loaded skill",
            "Failed to load skill",
        ),
        "read_skill_file" => (
            ActivityKind::Skill,
            "Reading skill file",
            "Read skill file",
            "Failed to read skill file",
        ),
        "session_create" => (
            ActivityKind::Other,
            "Creating delegated session",
            "Created delegated session",
            "Failed to create delegated session",
        ),
        "session_list" => (
            ActivityKind::Read,
            "Listing sessions",
            "Listed sessions",
            "Failed to list sessions",
        ),
        "session_status" => (
            ActivityKind::Read,
            "Checking session status",
            "Checked session status",
            "Failed to check session status",
        ),
        "session_messages" => (
            ActivityKind::Read,
            "Reading session messages",
            "Read session messages",
            "Failed to read session messages",
        ),
        "session_send" => (
            ActivityKind::Other,
            "Sending to session",
            "Sent to session",
            "Failed to send to session",
        ),
        "session_stop" => (
            ActivityKind::Other,
            "Stopping session turn",
            "Stopped session turn",
            "Failed to stop session turn",
        ),
        "session_wait" => (
            ActivityKind::Read,
            "Waiting for sessions",
            "Observed session change",
            "Failed while waiting for sessions",
        ),
        _ => (
            ActivityKind::Other,
            "Using tool",
            "Used tool",
            "Tool failed",
        ),
    }
}

fn activity_detail(tool: &str, arguments: &str) -> String {
    let Ok(arguments) = serde_json::from_str::<Value>(arguments) else {
        return tool.to_owned();
    };
    match tool {
        "search" => {
            let query = string_arg(&arguments, "query").unwrap_or_default();
            let path = string_arg(&arguments, "path").unwrap_or(".");
            format!("{query:?} in {path}")
        }
        "list_files" => string_arg(&arguments, "path").unwrap_or(".").to_owned(),
        "shell" | "shell_noninteractive" => {
            string_arg(&arguments, "command").unwrap_or(tool).to_owned()
        }
        "terminal_input" | "terminal_read" | "terminal_close" => {
            string_arg(&arguments, "terminal_id")
                .unwrap_or(tool)
                .to_owned()
        }
        "load_skill" => string_arg(&arguments, "name").unwrap_or(tool).to_owned(),
        "session_create" => "fresh isolated context".into(),
        "session_list" => "controllable sessions".into(),
        "session_status" => arguments
            .get("session_ids")
            .and_then(Value::as_array)
            .map(|ids| format!("{} session(s)", ids.len()))
            .unwrap_or_else(|| tool.to_owned()),
        "session_messages" | "session_send" | "session_stop" => {
            string_arg(&arguments, "session_id")
                .unwrap_or(tool)
                .to_owned()
        }
        "session_wait" => arguments
            .get("targets")
            .and_then(Value::as_array)
            .map(|targets| format!("{} session(s)", targets.len()))
            .unwrap_or_else(|| tool.to_owned()),
        _ => ["path", "name", "query", "command"]
            .into_iter()
            .find_map(|key| string_arg(&arguments, key))
            .unwrap_or(tool)
            .to_owned(),
    }
}

fn string_arg<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activities_have_shared_human_readable_lifecycle_labels() {
        let mut activity = AgentActivity::started(
            "call-1".into(),
            Uuid::new_v4(),
            3,
            7,
            "read_file",
            r#"{"path":"src/main.rs"}"#,
        );
        assert_eq!(activity.title, "Reading");
        assert_eq!(activity.detail, "src/main.rs");
        assert_eq!(activity.status, ActivityStatus::Running);
        assert_eq!(activity.sequence, Some(7));
        assert!(activity.started_at.is_some());
        assert!(activity.completed_at.is_none());

        activity.finish(true);
        assert_eq!(activity.title, "Read");
        assert_eq!(activity.status, ActivityStatus::Completed);
        assert!(activity.completed_at.is_some());
    }

    #[test]
    fn activity_details_preserve_complete_tool_arguments() {
        let command = "x".repeat(400);
        let arguments = serde_json::json!({ "command": command }).to_string();
        let activity =
            AgentActivity::started("call-1".into(), Uuid::new_v4(), 0, 4, "shell", &arguments);

        assert_eq!(activity.detail, command);
    }
}
