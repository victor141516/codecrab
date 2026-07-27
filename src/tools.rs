use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::{
    process::Command,
    sync::{mpsc, oneshot},
    time::timeout,
};
use walkdir::WalkDir;

use crate::events::AgentEvent;

#[derive(Clone, Copy)]
pub(crate) enum ApprovalMode {
    Ask,
    Always,
    Never,
}

pub(crate) struct ToolBox {
    root: PathBuf,
    approval: ApprovalMode,
}

impl ToolBox {
    pub(crate) fn new(root: PathBuf, approval: ApprovalMode) -> Self {
        Self { root, approval }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn definitions(&self) -> Vec<Value> {
        vec![
            tool(
                "list_files",
                "List project files and directories. Use this before guessing paths.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Relative directory, defaults to ."},
                        "max_depth": {"type": "integer", "minimum": 1, "maximum": 8}
                    }
                }),
            ),
            tool(
                "read_file",
                "Read a UTF-8 text file with line numbers.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "start_line": {"type": "integer", "minimum": 1},
                        "end_line": {"type": "integer", "minimum": 1}
                    },
                    "required": ["path"]
                }),
            ),
            tool(
                "search",
                "Search text recursively in project files. Returns file, line and matching text.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "path": {"type": "string", "description": "Relative file or directory, defaults to ."}
                    },
                    "required": ["query"]
                }),
            ),
            tool(
                "write_file",
                "Create or completely overwrite a UTF-8 text file. Requires approval.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
            ),
            tool(
                "replace_in_file",
                "Replace one exact, unique string in a UTF-8 file. Requires approval.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old": {"type": "string"},
                        "new": {"type": "string"}
                    },
                    "required": ["path", "old", "new"]
                }),
            ),
            tool(
                "shell",
                "Run a shell command in the project directory. Requires approval.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 300}
                    },
                    "required": ["command"]
                }),
            ),
        ]
    }

    pub(crate) async fn execute(
        &self,
        name: &str,
        args: &str,
        events: Option<&mpsc::UnboundedSender<AgentEvent>>,
    ) -> Value {
        let parsed: Value = match serde_json::from_str(args) {
            Ok(value) => value,
            Err(error) => {
                return json!({"ok": false, "error": format!("invalid arguments: {error}")});
            }
        };
        let result = match name {
            "list_files" => self.list_files(&parsed),
            "read_file" => self.read_file(&parsed),
            "search" => self.search(&parsed),
            "write_file" => self.write_file(&parsed, events).await,
            "replace_in_file" => self.replace_in_file(&parsed, events).await,
            "shell" => self.shell(&parsed, events).await,
            _ => Err(anyhow::anyhow!("unknown tool {name:?}")),
        };
        match result {
            Ok(value) => json!({"ok": true, "result": value}),
            Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
        }
    }

    fn list_files(&self, args: &Value) -> Result<Value> {
        let relative = string_arg_or(args, "path", ".");
        let path = self.existing_path(relative)?;
        if !path.is_dir() {
            anyhow::bail!("{relative:?} is not a directory");
        }
        let max_depth = args
            .get("max_depth")
            .and_then(Value::as_u64)
            .unwrap_or(3)
            .clamp(1, 8) as usize;
        let mut entries = Vec::new();
        for entry in WalkDir::new(&path)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|e| !ignored(e.path()))
            .filter_map(Result::ok)
            .take(1000)
        {
            if entry.path() == path {
                continue;
            }
            let mut display = entry
                .path()
                .strip_prefix(&self.root)
                .expect("walked path is inside project root")
                .display()
                .to_string();
            if entry.file_type().is_dir() {
                display.push('/');
            }
            entries.push(display.replace('\\', "/"));
        }
        Ok(json!({"entries": entries, "truncated": entries.len() == 1000}))
    }

    fn read_file(&self, args: &Value) -> Result<Value> {
        let relative = required_string(args, "path")?;
        let path = self.existing_path(relative)?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("{} is not a readable UTF-8 file", relative))?;
        let start = args
            .get("start_line")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let end = args
            .get("end_line")
            .and_then(Value::as_u64)
            .unwrap_or(start.saturating_add(399) as u64) as usize;
        let lines: Vec<_> = text.lines().collect();
        let selected = lines
            .iter()
            .enumerate()
            .skip(start - 1)
            .take(end.saturating_sub(start).saturating_add(1))
            .map(|(index, line)| format!("{:>6} | {line}", index + 1))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(json!({
            "content": truncate(selected, 40_000),
            "total_lines": lines.len(),
            "start_line": start,
            "end_line": end.min(lines.len())
        }))
    }

    fn search(&self, args: &Value) -> Result<Value> {
        let query = required_string(args, "query")?;
        if query.is_empty() {
            anyhow::bail!("query cannot be empty");
        }
        let relative = string_arg_or(args, "path", ".");
        let path = self.existing_path(relative)?;
        let walker = if path.is_file() {
            WalkDir::new(&path).max_depth(0)
        } else {
            WalkDir::new(&path)
        };
        let mut matches = Vec::new();
        for entry in walker
            .into_iter()
            .filter_entry(|e| !ignored(e.path()))
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let Ok(text) = fs::read_to_string(entry.path()) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if line.contains(query) {
                    matches.push(json!({
                        "path": entry.path().strip_prefix(&self.root)
                            .expect("walked path is inside project root")
                            .display().to_string().replace('\\', "/"),
                        "line": index + 1,
                        "text": truncate(line.to_owned(), 500)
                    }));
                    if matches.len() == 250 {
                        return Ok(json!({"matches": matches, "truncated": true}));
                    }
                }
            }
        }
        Ok(json!({"matches": matches, "truncated": false}))
    }

    async fn write_file(
        &self,
        args: &Value,
        events: Option<&mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<Value> {
        let relative = required_string(args, "path")?;
        let content = required_string(args, "content")?;
        let path = self.new_path(relative)?;
        self.approve(
            &format!("write {} ({} bytes)", relative, content.len()),
            events,
        )
        .await?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(json!({"path": relative, "bytes": content.len()}))
    }

    async fn replace_in_file(
        &self,
        args: &Value,
        events: Option<&mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<Value> {
        let relative = required_string(args, "path")?;
        let old = required_string(args, "old")?;
        let new = required_string(args, "new")?;
        if old.is_empty() {
            anyhow::bail!("old cannot be empty");
        }
        let path = self.existing_path(relative)?;
        let text = fs::read_to_string(&path)?;
        let occurrences = text.matches(old).count();
        if occurrences != 1 {
            anyhow::bail!("expected exactly one match, found {occurrences}");
        }
        self.approve(&format!("edit {relative}"), events).await?;
        fs::write(&path, text.replacen(old, new, 1))?;
        Ok(json!({"path": relative, "replacements": 1}))
    }

    async fn shell(
        &self,
        args: &Value,
        events: Option<&mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<Value> {
        let command = required_string(args, "command")?;
        let seconds = args
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(120)
            .clamp(1, 300);
        self.approve(&format!("run: {command}"), events).await?;

        #[cfg(windows)]
        let mut child = {
            let mut cmd = Command::new("powershell");
            cmd.args(["-NoLogo", "-NoProfile", "-Command", command]);
            cmd
        };
        #[cfg(not(windows))]
        let mut child = {
            let mut cmd = Command::new("sh");
            cmd.args(["-lc", command]);
            cmd
        };
        child
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = timeout(Duration::from_secs(seconds), child.output())
            .await
            .context("command timed out")??;
        Ok(json!({
            "exit_code": output.status.code(),
            "stdout": truncate(String::from_utf8_lossy(&output.stdout).into_owned(), 30_000),
            "stderr": truncate(String::from_utf8_lossy(&output.stderr).into_owned(), 30_000)
        }))
    }

    async fn approve(
        &self,
        action: &str,
        events: Option<&mpsc::UnboundedSender<AgentEvent>>,
    ) -> Result<()> {
        match self.approval {
            ApprovalMode::Always => Ok(()),
            ApprovalMode::Never => {
                anyhow::bail!("approval required for {action}; rerun non-interactively with --yes")
            }
            ApprovalMode::Ask => {
                let events = events.context("approval UI is unavailable")?;
                let (response, answer) = oneshot::channel();
                events
                    .send(AgentEvent::ApprovalRequested {
                        action: action.to_owned(),
                        response,
                    })
                    .map_err(|_| anyhow::anyhow!("approval UI is unavailable"))?;
                match answer.await {
                    Ok(true) => Ok(()),
                    Ok(false) => anyhow::bail!("user denied approval"),
                    Err(_) => anyhow::bail!("approval UI closed"),
                }
            }
        }
    }

    fn existing_path(&self, relative: &str) -> Result<PathBuf> {
        validate_relative(relative)?;
        let path = self
            .root
            .join(relative)
            .canonicalize()
            .with_context(|| format!("path does not exist: {relative}"))?;
        self.ensure_inside(path)
    }

    fn new_path(&self, relative: &str) -> Result<PathBuf> {
        validate_relative(relative)?;
        let joined = self.root.join(relative);
        let mut ancestor = joined.as_path();
        while !ancestor.exists() {
            ancestor = ancestor.parent().context("path has no existing parent")?;
        }
        let canonical = ancestor.canonicalize()?;
        if !canonical.starts_with(&self.root) {
            anyhow::bail!("path escapes the project root");
        }
        Ok(joined)
    }

    fn ensure_inside(&self, path: PathBuf) -> Result<PathBuf> {
        if path.starts_with(&self.root) {
            Ok(path)
        } else {
            anyhow::bail!("path escapes the project root")
        }
    }
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

fn required_string<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument {name:?}"))
}

fn string_arg_or<'a>(args: &'a Value, name: &str, default: &'a str) -> &'a str {
    args.get(name).and_then(Value::as_str).unwrap_or(default)
}

fn validate_relative(path: &str) -> Result<()> {
    let path = Path::new(path);
    if path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        anyhow::bail!("only relative paths inside the project are allowed");
    }
    Ok(())
}

fn ignored(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| matches!(name, ".git" | ".codecrab" | "target" | "node_modules"))
}

fn truncate(mut value: String, max: usize) -> String {
    if value.len() <= max {
        return value;
    }
    let mut boundary = max;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str("\n… output truncated …");
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_outside_root() {
        assert!(validate_relative("../secret").is_err());
        assert!(validate_relative("/etc/passwd").is_err());
        assert!(validate_relative("src/main.rs").is_ok());
    }

    #[tokio::test]
    async fn exact_replace_rejects_ambiguous_match() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.txt"), "same same").unwrap();
        let toolbox = ToolBox::new(temp.path().canonicalize().unwrap(), ApprovalMode::Always);
        let result = toolbox
            .replace_in_file(
                &json!({
                    "path": "a.txt", "old": "same", "new": "different"
                }),
                None,
            )
            .await;
        assert!(result.is_err());
    }
}
