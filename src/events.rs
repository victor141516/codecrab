use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::provider::Message;

#[derive(Clone)]
pub(crate) enum AgentEvent {
    AssistantMessage(Message),
    AssistantTextDelta { delta: String, start: bool },
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
    pub turn_message_index: usize,
    pub tool: String,
    pub kind: ActivityKind,
    pub status: ActivityStatus,
    pub title: String,
    pub detail: String,
}

impl AgentActivity {
    pub(crate) fn started(
        id: String,
        turn_message_index: usize,
        tool: &str,
        arguments: &str,
    ) -> Self {
        let (kind, running, _, _) = activity_labels(tool);
        Self {
            id,
            turn_message_index,
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
    }

    pub(crate) fn model_retry(
        id: String,
        turn_message_index: usize,
        retry: usize,
        max_retries: usize,
        error: String,
    ) -> Self {
        Self {
            id,
            turn_message_index,
            tool: "model_request".into(),
            kind: ActivityKind::Network,
            status: ActivityStatus::Completed,
            title: format!("Retrying model request ({retry}/{max_retries})"),
            detail: error,
        }
    }

    pub(crate) fn model_error(id: String, turn_message_index: usize, error: String) -> Self {
        Self {
            id,
            turn_message_index,
            tool: "model_request".into(),
            kind: ActivityKind::Network,
            status: ActivityStatus::Failed,
            title: "Model request failed".into(),
            detail: error,
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
        "shell" => (ActivityKind::Shell, "Running", "Ran", "Command failed"),
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
        "shell" => string_arg(&arguments, "command").unwrap_or(tool).to_owned(),
        "load_skill" => string_arg(&arguments, "name").unwrap_or(tool).to_owned(),
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
        let mut activity =
            AgentActivity::started("call-1".into(), 3, "read_file", r#"{"path":"src/main.rs"}"#);
        assert_eq!(activity.title, "Reading");
        assert_eq!(activity.detail, "src/main.rs");
        assert_eq!(activity.status, ActivityStatus::Running);

        activity.finish(true);
        assert_eq!(activity.title, "Read");
        assert_eq!(activity.status, ActivityStatus::Completed);
    }

    #[test]
    fn activity_details_preserve_complete_tool_arguments() {
        let command = "x".repeat(400);
        let arguments = serde_json::json!({ "command": command }).to_string();
        let activity = AgentActivity::started("call-1".into(), 0, "shell", &arguments);

        assert_eq!(activity.detail, command);
    }
}
