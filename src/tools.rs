use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};
use walkdir::WalkDir;

use crate::{
    coordination::SessionControl,
    terminal::{TerminalManager, TerminalRecord},
};

pub(crate) struct ToolBox {
    root: PathBuf,
    terminals: TerminalManager,
    session_control: Option<SessionControl>,
}

impl ToolBox {
    #[cfg(test)]
    pub(crate) fn new(root: PathBuf) -> Self {
        Self::with_shell(root, None)
    }

    #[cfg(test)]
    pub(crate) fn with_shell(root: PathBuf, shell: Option<String>) -> Self {
        Self {
            terminals: TerminalManager::new(root.clone(), shell),
            root,
            session_control: None,
        }
    }

    pub(crate) fn with_session_control(
        root: PathBuf,
        shell: Option<String>,
        session_control: SessionControl,
    ) -> Self {
        Self {
            terminals: TerminalManager::new(root.clone(), shell),
            root,
            session_control: Some(session_control),
        }
    }

    pub(crate) fn restore_terminals(&self, records: &[TerminalRecord], next_id: u64) {
        self.terminals.restore(records, next_id);
    }

    pub(crate) fn terminal_state(&self) -> (u64, Vec<TerminalRecord>) {
        self.terminals.persisted_state()
    }

    pub(crate) fn close_terminals(&self) -> Result<()> {
        self.terminals.close_all()
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn definitions(&self) -> Vec<Value> {
        let mut definitions = vec![
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
                "Run a command in a managed PTY. A command still running after five seconds returns a reusable terminal ID.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"}
                    },
                    "required": ["command"]
                }),
            ),
            tool(
                "shell_noninteractive",
                "Run a non-interactive command with separate stdout and stderr.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 300}
                    },
                    "required": ["command"]
                }),
            ),
            tool(
                "terminal_input",
                "Send ordered semantic actions to a terminal, then observe it. Actions: text {text}; paste {text}; key {key, modifiers?}; mouse {action, button, row, column, modifiers?}; resize {columns, rows}. Key names include Enter, ArrowUp, Escape, F1, and Character:a.",
                json!({
                    "type": "object",
                    "properties": {
                        "terminal_id": {"type": "string"},
                        "actions": {
                            "type": "array",
                            "minItems": 1,
                            "items": {"type": "object"}
                        }
                    },
                    "required": ["terminal_id", "actions"]
                }),
            ),
            tool(
                "terminal_read",
                "Observe a managed terminal without sending input.",
                json!({
                    "type": "object",
                    "properties": {
                        "terminal_id": {"type": "string"}
                    },
                    "required": ["terminal_id"]
                }),
            ),
            tool(
                "terminal_close",
                "Force-close and reap a managed terminal process tree.",
                json!({
                    "type": "object",
                    "properties": {
                        "terminal_id": {"type": "string"}
                    },
                    "required": ["terminal_id"]
                }),
            ),
            tool(
                "terminal_list",
                "List all managed terminals registered to this conversation.",
                json!({"type": "object", "properties": {}}),
            ),
        ];
        if self.session_control.is_some() {
            definitions.extend(SessionControl::definitions());
        }
        definitions
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
            "shell_noninteractive" => self.shell_noninteractive(&parsed).await,
            "terminal_input" => self.terminal_input(&parsed).await,
            "terminal_read" => self.terminal_read(&parsed).await,
            "terminal_close" => self.terminal_close(&parsed),
            "terminal_list" => Ok(self.terminals.list()),
            name if name.starts_with("session_") => match &self.session_control {
                Some(control) => control.execute(name, &parsed).await,
                None => Err(anyhow::anyhow!("session control is unavailable")),
            },
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
        self.terminals.shell(command).await
    }

    async fn shell_noninteractive(&self, args: &Value) -> Result<Value> {
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
        #[cfg(unix)]
        child.process_group(0);
        child
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = child
            .spawn()
            .context("cannot start non-interactive command")?;
        let mut process_tree = NonInteractiveProcessTree::new(&child);
        let mut stdout = child.stdout.take().context("stdout pipe is unavailable")?;
        let mut stderr = child.stderr.take().context("stderr pipe is unavailable")?;
        let stdout_reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).await.map(|_| bytes)
        });
        let stderr_reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await.map(|_| bytes)
        });
        let (status, timed_out) = match timeout(Duration::from_secs(seconds), child.wait()).await {
            Ok(status) => (status?, false),
            Err(_) => {
                process_tree.terminate();
                let _ = child.kill().await;
                (child.wait().await?, true)
            }
        };
        process_tree.disarm();
        let stdout = stdout_reader.await.context("stdout reader task failed")??;
        let stderr = stderr_reader.await.context("stderr reader task failed")??;
        Ok(json!({
            "exit_code": status.code(),
            "timed_out": timed_out,
            "stdout": truncate(String::from_utf8_lossy(&stdout).into_owned(), 30_000),
            "stderr": truncate(String::from_utf8_lossy(&stderr).into_owned(), 30_000)
        }))
    }

    async fn terminal_input(&self, args: &Value) -> Result<Value> {
        let terminal_id = required_string(args, "terminal_id")?;
        let actions = args
            .get("actions")
            .and_then(Value::as_array)
            .context("missing array argument \"actions\"")?;
        self.terminals.input(terminal_id, actions).await
    }

    async fn terminal_read(&self, args: &Value) -> Result<Value> {
        self.terminals
            .read(required_string(args, "terminal_id")?)
            .await
    }

    fn terminal_close(&self, args: &Value) -> Result<Value> {
        self.terminals.close(required_string(args, "terminal_id")?)
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

#[cfg(unix)]
struct NonInteractiveProcessTree {
    process_group: Option<libc::pid_t>,
    child_pid: Option<libc::pid_t>,
    armed: bool,
}

#[cfg(unix)]
impl NonInteractiveProcessTree {
    fn new(child: &tokio::process::Child) -> Self {
        let child_pid = child.id().map(|id| id as libc::pid_t);
        Self {
            process_group: child_pid,
            child_pid,
            armed: true,
        }
    }

    fn terminate(&self) {
        if self.armed
            && let Some(process_group) = self.process_group
        {
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn reap(&self) {
        if let Some(child_pid) = self.child_pid {
            loop {
                let result = unsafe { libc::waitpid(child_pid, std::ptr::null_mut(), 0) };
                if result >= 0
                    || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    break;
                }
            }
        }
    }
}

#[cfg(unix)]
impl Drop for NonInteractiveProcessTree {
    fn drop(&mut self) {
        if self.armed {
            self.terminate();
            self.reap();
        }
    }
}

#[cfg(windows)]
struct NonInteractiveProcessTree {
    job: usize,
    process: usize,
    armed: bool,
}

#[cfg(windows)]
impl NonInteractiveProcessTree {
    fn new(child: &tokio::process::Child) -> Self {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let Some(process) = child.raw_handle() else {
            return Self {
                job: 0,
                process: 0,
                armed: false,
            };
        };
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Self {
                    job: 0,
                    process: 0,
                    armed: false,
                };
            }
            let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &information as *const _ as *const _,
                std::mem::size_of_val(&information) as u32,
            ) == 0
                || AssignProcessToJobObject(job, process) == 0
            {
                windows_sys::Win32::Foundation::CloseHandle(job);
                return Self {
                    job: 0,
                    process: 0,
                    armed: false,
                };
            }
            Self {
                job: job as usize,
                process: process as usize,
                armed: true,
            }
        }
    }

    fn terminate(&self) {
        if self.armed && self.job != 0 {
            unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job as _, 1);
            }
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn reap(&self) {
        if self.process != 0 {
            unsafe {
                windows_sys::Win32::System::Threading::WaitForSingleObject(
                    self.process as _,
                    5_000,
                );
            }
        }
    }
}

#[cfg(windows)]
impl Drop for NonInteractiveProcessTree {
    fn drop(&mut self) {
        if self.armed {
            self.terminate();
            self.reap();
        }
        if self.job != 0 {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.job as _);
            }
        }
    }
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

    #[tokio::test]
    async fn noninteractive_timeout_returns_partial_separate_output() {
        let temp = tempfile::tempdir().unwrap();
        let toolbox = ToolBox::new(temp.path().canonicalize().unwrap());
        let command = if cfg!(windows) {
            "Write-Output 'partial-out'; [Console]::Error.WriteLine('partial-err'); Start-Sleep 10"
        } else {
            "printf 'partial-out\\n'; printf 'partial-err\\n' >&2; sleep 10"
        };

        let result = toolbox
            .execute(
                "shell_noninteractive",
                &json!({"command": command, "timeout_seconds": 1}).to_string(),
            )
            .await;

        assert_eq!(result["ok"], true);
        assert_eq!(result["result"]["timed_out"], true);
        assert!(
            result["result"]["stdout"]
                .as_str()
                .unwrap()
                .contains("partial-out")
        );
        assert!(
            result["result"]["stderr"]
                .as_str()
                .unwrap()
                .contains("partial-err")
        );
    }
}
