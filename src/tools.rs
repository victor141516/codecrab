use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::{process::Command, time::timeout};
use walkdir::WalkDir;

pub(crate) struct ToolBox {
    root: PathBuf,
}

impl ToolBox {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn definitions(&self) -> Vec<Value> {
        vec![
            tool(
                "list_files",
                "List files and directories. Relative paths start at the working directory; parent and absolute paths are allowed.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Directory path, defaults to the working directory"},
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
                "Search text recursively in files. Relative paths start at the working directory; parent and absolute paths are allowed.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "path": {"type": "string", "description": "File or directory path, defaults to the working directory"}
                    },
                    "required": ["query"]
                }),
            ),
            tool(
                "write_file",
                "Create or completely overwrite a UTF-8 text file.",
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
                "Replace one exact, unique string in a UTF-8 file.",
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
                "Run a shell command in the project directory.",
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

    pub(crate) async fn execute(&self, name: &str, args: &str) -> Value {
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
            "write_file" => self.write_file(&parsed),
            "replace_in_file" => self.replace_in_file(&parsed),
            "shell" => self.shell(&parsed).await,
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
            .filter_map(Result::ok)
            .take(1000)
        {
            if entry.path() == path {
                continue;
            }
            let mut display = self.display_path(entry.path());
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
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let Ok(text) = fs::read_to_string(entry.path()) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if line.contains(query) {
                    matches.push(json!({
                        "path": self.display_path(entry.path()).replace('\\', "/"),
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

    fn write_file(&self, args: &Value) -> Result<Value> {
        let relative = required_string(args, "path")?;
        let content = required_string(args, "content")?;
        let path = self.new_path(relative)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(json!({"path": relative, "bytes": content.len()}))
    }

    fn replace_in_file(&self, args: &Value) -> Result<Value> {
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
        fs::write(&path, text.replacen(old, new, 1))?;
        Ok(json!({"path": relative, "replacements": 1}))
    }

    async fn shell(&self, args: &Value) -> Result<Value> {
        let command = required_string(args, "command")?;
        let seconds = args
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(120)
            .clamp(1, 300);
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

    fn existing_path(&self, path: &str) -> Result<PathBuf> {
        let path = self
            .root
            .join(path)
            .canonicalize()
            .with_context(|| format!("path does not exist: {path}"))?;
        Ok(path)
    }

    fn new_path(&self, path: &str) -> Result<PathBuf> {
        Ok(self.root.join(path))
    }

    fn display_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
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
    fn reads_and_writes_paths_outside_the_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let toolbox = ToolBox::new(project.canonicalize().unwrap());

        toolbox
            .write_file(&json!({"path": "../outside.txt", "content": "parent"}))
            .unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("outside.txt")).unwrap(),
            "parent"
        );

        let absolute = temp.path().join("absolute.txt");
        fs::write(&absolute, "absolute").unwrap();
        let result = toolbox
            .read_file(&json!({"path": absolute.to_string_lossy()}))
            .unwrap();
        assert_eq!(result["content"], "     1 | absolute");
    }

    #[test]
    fn exact_replace_rejects_ambiguous_match() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.txt"), "same same").unwrap();
        let toolbox = ToolBox::new(temp.path().canonicalize().unwrap());
        let result = toolbox.replace_in_file(&json!({
            "path": "a.txt", "old": "same", "new": "different"
        }));
        assert!(result.is_err());
    }
}
