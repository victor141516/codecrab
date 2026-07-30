pub(crate) mod constants;

use std::{
    collections::HashMap,
    ffi::OsStr,
    io::Read,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    thread,
    time::Instant,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tattoy_wezterm_term::{
    CellAttributes, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind, Terminal,
    TerminalConfiguration, TerminalSize, color::ColorPalette,
};
use tokio::time::{sleep, timeout_at};

use self::constants::{
    CHILD_POLL_INTERVAL, DEFAULT_COLUMNS, DEFAULT_ROWS, FOLLOW_UP_DEADLINE, INITIAL_OBSERVATION,
    MAX_TERMINALS_PER_SESSION, MODEL_TRANSCRIPT_TAIL, READER_BUFFER_BYTES, SCREEN_STABILITY,
    SCROLLBACK_ROWS,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalProcessState {
    Running,
    Exited,
    Closed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservationClassification {
    #[default]
    Unchanged,
    ChangedThenStabilized,
    StillChangingAtDeadline,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TerminalStyle {
    pub foreground: String,
    pub background: String,
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub underline: String,
    pub reverse: bool,
    pub strikethrough: bool,
    pub invisible: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TerminalStyleSpan {
    pub text: String,
    pub style: TerminalStyle,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TerminalScreenLine {
    pub spans: Vec<TerminalStyleSpan>,
    pub wrapped: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TerminalCursor {
    pub row: usize,
    pub column: usize,
    pub shape: String,
    pub visible: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TerminalScreenSnapshot {
    pub terminal_id: String,
    pub screen_sequence: u64,
    pub rows: usize,
    pub columns: usize,
    pub lines: Vec<TerminalScreenLine>,
    pub cursor: TerminalCursor,
    pub alternate_screen: bool,
    pub title: String,
    pub mouse_reporting: bool,
    pub bracketed_paste: bool,
    pub process_state: TerminalProcessState,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
    pub observation: ObservationClassification,
    pub recent_transcript: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TerminalRecord {
    pub id: String,
    pub command: String,
    pub shell: String,
    pub working_directory: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    pub columns: u16,
    pub rows: u16,
    pub state: TerminalProcessState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_snapshot: Option<TerminalScreenSnapshot>,
    #[serde(default)]
    pub latest_observation: ObservationClassification,
    #[serde(default)]
    pub recent_transcript: String,
}

#[derive(Clone)]
pub(crate) struct TerminalManager {
    root: PathBuf,
    configured_shell: Option<String>,
    inner: Arc<Mutex<ManagerState>>,
}

#[derive(Default)]
struct ManagerState {
    actors: HashMap<String, Arc<TerminalActor>>,
    records: HashMap<String, TerminalRecord>,
    next_id: u64,
    starting: usize,
}

struct TerminalActor {
    id: String,
    command: String,
    shell: String,
    working_directory: PathBuf,
    created_at: DateTime<Utc>,
    core: Arc<Mutex<ActorCore>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
    process_tree: ProcessTree,
}

struct ActorCore {
    emulator: TerminalEmulator,
    sequence: u64,
    last_change: Instant,
    reader_eof: bool,
    closed: bool,
}

#[derive(Debug)]
struct EmulatorConfig;

impl TerminalConfiguration for EmulatorConfig {
    fn scrollback_size(&self) -> usize {
        SCROLLBACK_ROWS
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

/// Internal boundary around the selected VT emulator implementation.
///
/// Tool, persistence, and client contracts depend only on the serializable
/// terminal types in this module, so the emulator crate can be replaced
/// without changing those contracts.
struct TerminalEmulator(Terminal);

impl TerminalEmulator {
    fn new(columns: u16, rows: u16, writer: Box<dyn std::io::Write + Send>) -> TerminalEmulator {
        Self(Terminal::new(
            terminal_size(columns, rows),
            Arc::new(EmulatorConfig),
            "CodeCrab",
            env!("CARGO_PKG_VERSION"),
            writer,
        ))
    }
}

impl Deref for TerminalEmulator {
    type Target = Terminal;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TerminalEmulator {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl TerminalManager {
    pub(crate) fn new(root: PathBuf, configured_shell: Option<String>) -> Self {
        Self {
            root,
            configured_shell,
            inner: Arc::new(Mutex::new(ManagerState {
                next_id: 1,
                ..ManagerState::default()
            })),
        }
    }

    pub(crate) fn restore(&self, records: &[TerminalRecord], next_id: u64) {
        let mut state = self.inner.lock().expect("terminal manager mutex poisoned");
        state.actors.clear();
        state.records.clear();
        for record in records {
            let mut record = record.clone();
            if record.state == TerminalProcessState::Running {
                record.state = TerminalProcessState::Interrupted;
                record.completed_at = Some(Utc::now());
                record.updated_at = Utc::now();
                if let Some(snapshot) = record.latest_snapshot.as_mut() {
                    snapshot.process_state = TerminalProcessState::Interrupted;
                }
            }
            state.records.insert(record.id.clone(), record);
        }
        state.next_id = next_id.max(next_terminal_id(records));
    }

    pub(crate) fn persisted_state(&self) -> (u64, Vec<TerminalRecord>) {
        self.refresh_all_records();
        let state = self.inner.lock().expect("terminal manager mutex poisoned");
        let mut records = state.records.values().cloned().collect::<Vec<_>>();
        records.sort_by_key(|record| terminal_number(&record.id));
        (state.next_id, records)
    }

    pub(crate) async fn shell(&self, command: &str) -> Result<Value> {
        if command.trim().is_empty() {
            anyhow::bail!("command cannot be empty");
        }
        let actor = self.start(command)?;
        let mut guard = StartedTerminalGuard {
            manager: Arc::downgrade(&self.inner),
            actor: actor.clone(),
            armed: true,
        };
        let observed = actor.observe(true, 0).await?;
        let record = self.update_record(&actor, observed)?;
        guard.armed = false;

        if record.state == TerminalProcessState::Exited {
            Ok(json!({
                "state": record.state,
                "exit_code": record.exit_code,
                "text": record.recent_transcript,
                "observation": record.latest_observation,
            }))
        } else {
            Ok(json!({
                "state": record.state,
                "terminal_id": record.id,
                "command": record.command,
                "observation": record.latest_observation,
                "screen": record.latest_snapshot,
                "recent_transcript": record.recent_transcript,
            }))
        }
    }

    pub(crate) async fn input(&self, terminal_id: &str, actions: &[Value]) -> Result<Value> {
        if actions.is_empty() {
            anyhow::bail!("actions cannot be empty");
        }
        let actor = self.live_actor(terminal_id)?;
        let before = actor.sequence();
        actor.apply_actions(actions)?;
        let observed = actor.observe(false, before).await?;
        let record = self.update_record(&actor, observed)?;
        Ok(json!({
            "state": record.state,
            "terminal_id": record.id,
            "observation": record.latest_observation,
            "screen": record.latest_snapshot,
            "recent_transcript": record.recent_transcript,
        }))
    }

    pub(crate) async fn read(&self, terminal_id: &str) -> Result<Value> {
        let actor = self
            .inner
            .lock()
            .expect("terminal manager mutex poisoned")
            .actors
            .get(terminal_id)
            .cloned();
        let Some(actor) = actor else {
            let mut record = self
                .record(terminal_id)
                .with_context(|| format!("unknown terminal {terminal_id:?}"))?;
            record.latest_observation = ObservationClassification::Unchanged;
            record.updated_at = Utc::now();
            if let Some(snapshot) = record.latest_snapshot.as_mut() {
                snapshot.observation = ObservationClassification::Unchanged;
            }
            self.inner
                .lock()
                .expect("terminal manager mutex poisoned")
                .records
                .insert(terminal_id.to_owned(), record.clone());
            return Ok(terminal_observation_value(&record));
        };
        if actor.poll_exit()?.is_some() {
            let record = self.update_record(&actor, ObservationClassification::Unchanged)?;
            return Ok(terminal_observation_value(&record));
        }
        let before = self
            .record(terminal_id)
            .and_then(|record| record.latest_snapshot)
            .map(|snapshot| snapshot.screen_sequence)
            .unwrap_or_else(|| actor.sequence());
        let observed = actor.observe(false, before).await?;
        let record = self.update_record(&actor, observed)?;
        Ok(terminal_observation_value(&record))
    }

    pub(crate) fn close(&self, terminal_id: &str) -> Result<Value> {
        let actor = self.actor(terminal_id)?;
        actor.close()?;
        let record = self.update_record(&actor, ObservationClassification::Unchanged)?;
        Ok(json!({
            "terminal_id": record.id,
            "state": record.state,
            "exit_code": record.exit_code,
        }))
    }

    pub(crate) fn list(&self) -> Value {
        let (_, records) = self.persisted_state();
        json!({
            "terminals": records.into_iter().map(|record| json!({
                "terminal_id": record.id,
                "command": record.command,
                "shell": record.shell,
                "state": record.state,
                "created_at": record.created_at,
                "updated_at": record.updated_at,
                "completed_at": record.completed_at,
                "exit_code": record.exit_code,
                "columns": record.columns,
                "rows": record.rows,
            })).collect::<Vec<_>>()
        })
    }

    pub(crate) fn close_all(&self) -> Result<()> {
        let actors = {
            self.inner
                .lock()
                .expect("terminal manager mutex poisoned")
                .actors
                .values()
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut first_error = None;
        for actor in actors {
            if let Err(error) = actor.close()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
            let _ = self.update_record(&actor, ObservationClassification::Unchanged);
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn clear(&self) -> Result<()> {
        self.close_all()?;
        let mut state = self.inner.lock().expect("terminal manager mutex poisoned");
        state.actors.clear();
        state.records.clear();
        state.next_id = 1;
        Ok(())
    }

    fn start(&self, command: &str) -> Result<Arc<TerminalActor>> {
        let shell = detect_shell(self.configured_shell.as_deref())?;
        let id = {
            let mut state = self.inner.lock().expect("terminal manager mutex poisoned");
            let live_count = state
                .records
                .values()
                .filter(|record| record.state == TerminalProcessState::Running)
                .count();
            if live_count + state.starting >= MAX_TERMINALS_PER_SESSION {
                anyhow::bail!(
                    "a conversation can have at most {MAX_TERMINALS_PER_SESSION} live terminals"
                );
            }
            let id = format!("terminal_{}", state.next_id);
            state.next_id = state.next_id.saturating_add(1);
            state.starting += 1;
            id
        };
        let result = (|| {
            let pair = native_pty_system().openpty(pty_size(DEFAULT_COLUMNS, DEFAULT_ROWS))?;
            let mut builder = shell.command(command);
            builder.cwd(&self.root);
            builder.env("TERM", "xterm-256color");
            builder.env("COLORTERM", "truecolor");
            let child = pair
                .slave
                .spawn_command(builder)
                .with_context(|| format!("cannot start shell {}", shell.program.display()))?;
            drop(pair.slave);

            let reader = pair.master.try_clone_reader()?;
            let writer = pair.master.take_writer()?;
            let process_tree = ProcessTree::new(pair.master.as_ref(), child.as_ref());
            let emulator = TerminalEmulator::new(DEFAULT_COLUMNS, DEFAULT_ROWS, writer);
            let core = Arc::new(Mutex::new(ActorCore {
                emulator,
                sequence: 0,
                last_change: Instant::now(),
                reader_eof: false,
                closed: false,
            }));
            spawn_reader(reader, core.clone(), id.clone())?;

            let actor = Arc::new(TerminalActor {
                id: id.clone(),
                command: command.to_owned(),
                shell: shell.program.display().to_string(),
                working_directory: self.root.clone(),
                created_at: Utc::now(),
                core,
                master: Mutex::new(Some(pair.master)),
                child: Mutex::new(Some(child)),
                process_tree,
            });
            let record = actor.record(ObservationClassification::Unchanged)?;
            Ok::<_, anyhow::Error>((actor, record))
        })();

        let mut state = self.inner.lock().expect("terminal manager mutex poisoned");
        state.starting = state.starting.saturating_sub(1);
        let (actor, record) = result?;
        state.actors.insert(id.clone(), actor.clone());
        state.records.insert(id, record);
        Ok(actor)
    }

    fn actor(&self, terminal_id: &str) -> Result<Arc<TerminalActor>> {
        self.inner
            .lock()
            .expect("terminal manager mutex poisoned")
            .actors
            .get(terminal_id)
            .cloned()
            .with_context(|| format!("terminal {terminal_id:?} is not attached to this process"))
    }

    fn live_actor(&self, terminal_id: &str) -> Result<Arc<TerminalActor>> {
        let actor = self.actor(terminal_id)?;
        if actor.poll_exit()?.is_some() {
            let record = self.update_record(&actor, ObservationClassification::Unchanged)?;
            anyhow::bail!(
                "terminal {terminal_id:?} is {}; only running terminals accept this operation",
                state_name(record.state)
            );
        }
        if let Some(record) = self.record(terminal_id)
            && record.state != TerminalProcessState::Running
        {
            anyhow::bail!(
                "terminal {terminal_id:?} is {}; only running terminals accept this operation",
                state_name(record.state)
            );
        }
        Ok(actor)
    }

    fn record(&self, terminal_id: &str) -> Option<TerminalRecord> {
        self.inner
            .lock()
            .expect("terminal manager mutex poisoned")
            .records
            .get(terminal_id)
            .cloned()
    }

    fn update_record(
        &self,
        actor: &Arc<TerminalActor>,
        observation: ObservationClassification,
    ) -> Result<TerminalRecord> {
        let mut record = actor.record(observation)?;
        let mut state = self.inner.lock().expect("terminal manager mutex poisoned");
        if record.completed_at.is_some()
            && let Some(previous) = state.records.get(&record.id)
            && previous.completed_at.is_some()
        {
            record.completed_at = previous.completed_at;
        }
        state.records.insert(record.id.clone(), record.clone());
        Ok(record)
    }

    fn refresh_all_records(&self) {
        let actors = {
            self.inner
                .lock()
                .expect("terminal manager mutex poisoned")
                .actors
                .values()
                .cloned()
                .collect::<Vec<_>>()
        };
        for actor in actors {
            let observation = self
                .record(&actor.id)
                .map(|record| record.latest_observation)
                .unwrap_or_default();
            let _ = self.update_record(&actor, observation);
        }
    }
}

impl TerminalActor {
    fn sequence(&self) -> u64 {
        self.core
            .lock()
            .expect("terminal emulator mutex poisoned")
            .sequence
    }

    async fn observe(
        &self,
        initial: bool,
        previous_sequence: u64,
    ) -> Result<ObservationClassification> {
        let duration = if initial {
            INITIAL_OBSERVATION
        } else {
            FOLLOW_UP_DEADLINE
        };
        let deadline = tokio::time::Instant::now() + duration;
        let mut changed = self.sequence() > previous_sequence;

        loop {
            if self.poll_exit()?.is_some() && initial {
                let drain_deadline = tokio::time::Instant::now() + SCREEN_STABILITY;
                while !self
                    .core
                    .lock()
                    .expect("terminal emulator mutex poisoned")
                    .reader_eof
                    && tokio::time::Instant::now() < drain_deadline
                {
                    sleep(CHILD_POLL_INTERVAL).await;
                }
                break;
            }
            let (sequence, stable_for) = {
                let core = self.core.lock().expect("terminal emulator mutex poisoned");
                (core.sequence, core.last_change.elapsed())
            };
            changed |= sequence > previous_sequence;
            if !initial && changed && stable_for >= SCREEN_STABILITY {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            let _ = timeout_at(deadline, sleep(CHILD_POLL_INTERVAL)).await;
        }

        let stable_for = self
            .core
            .lock()
            .expect("terminal emulator mutex poisoned")
            .last_change
            .elapsed();
        Ok(classify_observation(
            changed,
            stable_for,
            self.poll_exit()?.is_some(),
        ))
    }

    fn apply_actions(&self, actions: &[Value]) -> Result<()> {
        for action in actions {
            let action_type = required_string(action, "type")?;
            match action_type {
                "text" => {
                    let text = required_string(action, "text")?;
                    let mut core = self.core.lock().expect("terminal emulator mutex poisoned");
                    for character in text.chars() {
                        core.emulator
                            .key_down(KeyCode::Char(character), KeyModifiers::NONE)?;
                    }
                }
                "paste" => {
                    let text = required_string(action, "text")?;
                    self.core
                        .lock()
                        .expect("terminal emulator mutex poisoned")
                        .emulator
                        .send_paste(text)?;
                }
                "key" => {
                    let key = parse_key(required_string(action, "key")?)?;
                    let modifiers = parse_modifiers(action.get("modifiers"))?;
                    self.core
                        .lock()
                        .expect("terminal emulator mutex poisoned")
                        .emulator
                        .key_down(key, modifiers)?;
                }
                "mouse" => self.apply_mouse(action)?,
                "resize" => self.resize(action)?,
                other => anyhow::bail!("unknown terminal action type {other:?}"),
            }
        }
        Ok(())
    }

    fn apply_mouse(&self, action: &Value) -> Result<()> {
        let mut core = self.core.lock().expect("terminal emulator mutex poisoned");
        if !core.emulator.is_mouse_grabbed() {
            anyhow::bail!("mouse input is unavailable because the application did not enable it");
        }
        let size = core.emulator.get_size();
        let row = required_u64(action, "row")? as usize;
        let column = required_u64(action, "column")? as usize;
        if row == 0 || row > size.rows || column == 0 || column > size.cols {
            anyhow::bail!(
                "mouse position ({row},{column}) is outside the {}x{} terminal",
                size.rows,
                size.cols
            );
        }
        let event_action = required_string(action, "action")?;
        let button_name = action
            .get("button")
            .and_then(Value::as_str)
            .unwrap_or("none");
        let (kind, button) = match event_action {
            "press" => (MouseEventKind::Press, parse_mouse_button(button_name)?),
            "release" => (MouseEventKind::Release, parse_mouse_button(button_name)?),
            "move" => (MouseEventKind::Move, parse_mouse_button(button_name)?),
            "scroll_up" => (MouseEventKind::Press, MouseButton::WheelUp(1)),
            "scroll_down" => (MouseEventKind::Press, MouseButton::WheelDown(1)),
            other => anyhow::bail!("unknown mouse action {other:?}"),
        };
        core.emulator.mouse_event(MouseEvent {
            kind,
            x: column - 1,
            y: (row - 1) as i64,
            x_pixel_offset: 0,
            y_pixel_offset: 0,
            button,
            modifiers: parse_modifiers(action.get("modifiers"))?,
        })?;
        Ok(())
    }

    fn resize(&self, action: &Value) -> Result<()> {
        let columns = required_u64(action, "columns")?;
        let rows = required_u64(action, "rows")?;
        if !(1..=1000).contains(&columns) || !(1..=500).contains(&rows) {
            anyhow::bail!("terminal dimensions must be between 1x1 and 1000x500");
        }
        let columns = columns as u16;
        let rows = rows as u16;
        self.master
            .lock()
            .expect("terminal master mutex poisoned")
            .as_ref()
            .context("terminal PTY is closed")?
            .resize(pty_size(columns, rows))?;
        let mut core = self.core.lock().expect("terminal emulator mutex poisoned");
        core.emulator.resize(terminal_size(columns, rows));
        core.sequence = core.sequence.saturating_add(1);
        core.last_change = Instant::now();
        Ok(())
    }

    fn poll_exit(&self) -> Result<Option<u32>> {
        let mut child = self.child.lock().expect("terminal child mutex poisoned");
        let Some(child) = child.as_mut() else {
            return Ok(None);
        };
        Ok(child.try_wait()?.map(|status| status.exit_code()))
    }

    fn close(&self) -> Result<()> {
        let was_running = self.poll_exit()?.is_none();
        self.process_tree.terminate();
        if was_running {
            let mut child = self.child.lock().expect("terminal child mutex poisoned");
            if let Some(child) = child.as_mut() {
                let _ = child.kill();
                child.wait().context("cannot reap terminal process")?;
            }
        }
        self.master
            .lock()
            .expect("terminal master mutex poisoned")
            .take();
        if was_running {
            self.core
                .lock()
                .expect("terminal emulator mutex poisoned")
                .closed = true;
        }
        Ok(())
    }

    fn record(&self, observation: ObservationClassification) -> Result<TerminalRecord> {
        let exit_code = self.poll_exit()?;
        let state = if self
            .core
            .lock()
            .expect("terminal emulator mutex poisoned")
            .closed
        {
            TerminalProcessState::Closed
        } else if exit_code.is_some() {
            TerminalProcessState::Exited
        } else {
            TerminalProcessState::Running
        };
        let now = Utc::now();
        let core = self.core.lock().expect("terminal emulator mutex poisoned");
        let size = core.emulator.get_size();
        let transcript = transcript(&core.emulator);
        let snapshot = snapshot(
            &self.id,
            &self.command,
            &core,
            state,
            exit_code,
            observation,
            transcript.clone(),
        );
        Ok(TerminalRecord {
            id: self.id.clone(),
            command: self.command.clone(),
            shell: self.shell.clone(),
            working_directory: self.working_directory.clone(),
            created_at: self.created_at,
            updated_at: now,
            completed_at: (state != TerminalProcessState::Running).then_some(now),
            columns: size.cols as u16,
            rows: size.rows as u16,
            state,
            exit_code,
            latest_snapshot: Some(snapshot),
            latest_observation: observation,
            recent_transcript: transcript,
        })
    }
}

impl Drop for TerminalActor {
    fn drop(&mut self) {
        let running = self
            .child
            .get_mut()
            .ok()
            .and_then(Option::as_mut)
            .and_then(|child| child.try_wait().ok())
            .flatten()
            .is_none();
        if running {
            self.process_tree.terminate();
            if let Ok(Some(child)) = self.child.get_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

struct StartedTerminalGuard {
    manager: Weak<Mutex<ManagerState>>,
    actor: Arc<TerminalActor>,
    armed: bool,
}

impl Drop for StartedTerminalGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self.actor.close();
        if let Some(manager) = self.manager.upgrade() {
            let mut state = manager.lock().expect("terminal manager mutex poisoned");
            state.actors.remove(&self.actor.id);
            state.records.remove(&self.actor.id);
        }
    }
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    core: Arc<Mutex<ActorCore>>,
    terminal_id: String,
) -> Result<()> {
    thread::Builder::new()
        .name(format!("codecrab-{terminal_id}-reader"))
        .spawn(move || {
            let mut buffer = vec![0; READER_BUFFER_BYTES];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        core.lock()
                            .expect("terminal emulator mutex poisoned")
                            .reader_eof = true;
                        break;
                    }
                    Ok(read) => {
                        let mut core = core.lock().expect("terminal emulator mutex poisoned");
                        let before = screen_fingerprint(&core.emulator);
                        core.emulator.advance_bytes(&buffer[..read]);
                        if screen_fingerprint(&core.emulator) != before {
                            core.sequence = core.sequence.saturating_add(1);
                            core.last_change = Instant::now();
                        }
                    }
                    Err(_) => {
                        core.lock()
                            .expect("terminal emulator mutex poisoned")
                            .reader_eof = true;
                        break;
                    }
                }
            }
        })
        .context("cannot start terminal reader")?;
    Ok(())
}

fn snapshot(
    terminal_id: &str,
    command: &str,
    core: &ActorCore,
    state: TerminalProcessState,
    exit_code: Option<u32>,
    observation: ObservationClassification,
    recent_transcript: String,
) -> TerminalScreenSnapshot {
    let emulator = &core.emulator;
    let size = emulator.get_size();
    let mut lines = visible_lines(emulator)
        .into_iter()
        .map(|line| styled_line(&line))
        .collect::<Vec<_>>();
    while lines.last().is_some_and(is_blank_screen_line) {
        lines.pop();
    }
    let cursor = emulator.cursor_pos();
    TerminalScreenSnapshot {
        terminal_id: terminal_id.to_owned(),
        screen_sequence: core.sequence,
        rows: size.rows,
        columns: size.cols,
        lines,
        cursor: TerminalCursor {
            row: cursor.y.max(0) as usize + 1,
            column: cursor.x + 1,
            shape: format!("{:?}", cursor.shape).to_ascii_lowercase(),
            visible: format!("{:?}", cursor.visibility).eq_ignore_ascii_case("visible"),
        },
        alternate_screen: emulator.is_alt_screen_active(),
        title: emulator.get_title().to_owned(),
        mouse_reporting: emulator.is_mouse_grabbed(),
        bracketed_paste: emulator.bracketed_paste_enabled(),
        process_state: state,
        command: command.to_owned(),
        exit_code,
        observation,
        recent_transcript,
    }
}

fn styled_line(line: &tattoy_wezterm_term::Line) -> TerminalScreenLine {
    let mut spans: Vec<TerminalStyleSpan> = Vec::new();
    for cell in line.visible_cells() {
        let style = terminal_style(cell.attrs());
        let text = if cell.attrs().invisible() {
            " ".repeat(cell.width())
        } else {
            cell.str().to_owned()
        };
        if let Some(last) = spans.last_mut()
            && last.style == style
        {
            last.text.push_str(&text);
        } else {
            spans.push(TerminalStyleSpan { text, style });
        }
    }
    TerminalScreenLine {
        spans,
        wrapped: line.last_cell_was_wrapped(),
    }
}

fn terminal_style(attributes: &CellAttributes) -> TerminalStyle {
    let intensity = format!("{:?}", attributes.intensity()).to_ascii_lowercase();
    TerminalStyle {
        foreground: format!("{:?}", attributes.foreground()),
        background: format!("{:?}", attributes.background()),
        bold: intensity == "bold",
        faint: intensity == "half",
        italic: attributes.italic(),
        underline: format!("{:?}", attributes.underline()).to_ascii_lowercase(),
        reverse: attributes.reverse(),
        strikethrough: attributes.strikethrough(),
        invisible: attributes.invisible(),
    }
}

fn is_blank_screen_line(line: &TerminalScreenLine) -> bool {
    line.spans.iter().all(|span| span.text.trim().is_empty())
}

fn transcript(emulator: &Terminal) -> String {
    let mut text = String::new();
    emulator.screen().for_each_phys_line(|_, line| {
        for cell in line.visible_cells() {
            if cell.attrs().invisible() {
                text.extend(std::iter::repeat_n(' ', cell.width()));
            } else {
                text.push_str(cell.str());
            }
        }
        if !line.last_cell_was_wrapped() {
            text.push('\n');
        }
    });
    while text.ends_with([' ', '\n', '\r', '\t']) {
        text.pop();
    }
    tail_chars(text, MODEL_TRANSCRIPT_TAIL)
}

fn screen_fingerprint(emulator: &Terminal) -> String {
    let mut fingerprint = String::new();
    for line in visible_lines(emulator) {
        for cell in line.visible_cells() {
            fingerprint.push_str(cell.str());
            fingerprint.push_str(&format!("{:?}", cell.attrs()));
        }
        fingerprint.push(if line.last_cell_was_wrapped() {
            '\u{0}'
        } else {
            '\n'
        });
    }
    let cursor = emulator.cursor_pos();
    fingerprint.push_str(&format!(
        "\u{1}{}:{}:{:?}:{:?}:{}:{}:{}:{}",
        cursor.x,
        cursor.y,
        cursor.shape,
        cursor.visibility,
        emulator.get_title(),
        emulator.is_alt_screen_active(),
        emulator.is_mouse_grabbed(),
        emulator.bracketed_paste_enabled()
    ));
    fingerprint
}

fn tail_chars(text: String, maximum: usize) -> String {
    let count = text.chars().count();
    if count <= maximum {
        return text;
    }
    let byte = text
        .char_indices()
        .nth(count - maximum)
        .map(|(index, _)| index)
        .unwrap_or(0);
    format!("… transcript truncated …\n{}", &text[byte..])
}

fn visible_lines(emulator: &Terminal) -> Vec<tattoy_wezterm_term::Line> {
    let screen = emulator.screen();
    let end = screen.scrollback_rows();
    let start = end.saturating_sub(screen.physical_rows);
    screen.lines_in_phys_range(start..end)
}

fn parse_key(value: &str) -> Result<KeyCode> {
    if let Some(character) = value.strip_prefix("Character:") {
        let mut characters = character.chars();
        let character = characters.next().context("Character key is empty")?;
        if characters.next().is_some() {
            anyhow::bail!("Character key must contain exactly one character");
        }
        return Ok(KeyCode::Char(character));
    }
    let key = match value {
        "Enter" => KeyCode::Enter,
        "Escape" => KeyCode::Escape,
        "Tab" => KeyCode::Tab,
        "Backspace" => KeyCode::Backspace,
        "Delete" => KeyCode::Delete,
        "Insert" => KeyCode::Insert,
        "ArrowUp" => KeyCode::UpArrow,
        "ArrowDown" => KeyCode::DownArrow,
        "ArrowLeft" => KeyCode::LeftArrow,
        "ArrowRight" => KeyCode::RightArrow,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        other if other.starts_with('F') => {
            let number = other[1..]
                .parse::<u8>()
                .with_context(|| format!("invalid function key {other:?}"))?;
            if !(1..=24).contains(&number) {
                anyhow::bail!("function key must be between F1 and F24");
            }
            KeyCode::Function(number)
        }
        other => anyhow::bail!("unknown key {other:?}"),
    };
    Ok(key)
}

fn parse_modifiers(value: Option<&Value>) -> Result<KeyModifiers> {
    let mut parsed = KeyModifiers::NONE;
    let Some(values) = value else {
        return Ok(parsed);
    };
    let values = values.as_array().context("modifiers must be an array")?;
    for value in values {
        match value
            .as_str()
            .context("modifier names must be strings")?
            .to_ascii_lowercase()
            .as_str()
        {
            "shift" => parsed |= KeyModifiers::SHIFT,
            "alt" | "option" | "meta" => parsed |= KeyModifiers::ALT,
            "ctrl" | "control" => parsed |= KeyModifiers::CTRL,
            "super" | "cmd" | "win" => parsed |= KeyModifiers::SUPER,
            other => anyhow::bail!("unknown modifier {other:?}"),
        }
    }
    Ok(parsed)
}

fn parse_mouse_button(value: &str) -> Result<MouseButton> {
    Ok(match value {
        "left" => MouseButton::Left,
        "middle" => MouseButton::Middle,
        "right" => MouseButton::Right,
        "none" => MouseButton::None,
        other => anyhow::bail!("unknown mouse button {other:?}"),
    })
}

fn required_string<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string field {name:?}"))
}

fn required_u64(value: &Value, name: &str) -> Result<u64> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .with_context(|| format!("missing positive integer field {name:?}"))
}

fn pty_size(columns: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols: columns,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn terminal_size(columns: u16, rows: u16) -> TerminalSize {
    TerminalSize {
        rows: rows as usize,
        cols: columns as usize,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    }
}

fn state_name(state: TerminalProcessState) -> &'static str {
    match state {
        TerminalProcessState::Running => "running",
        TerminalProcessState::Exited => "exited",
        TerminalProcessState::Closed => "closed",
        TerminalProcessState::Interrupted => "interrupted",
    }
}

fn classify_observation(
    changed: bool,
    stable_for: std::time::Duration,
    process_exited: bool,
) -> ObservationClassification {
    if !changed {
        ObservationClassification::Unchanged
    } else if stable_for >= SCREEN_STABILITY || process_exited {
        ObservationClassification::ChangedThenStabilized
    } else {
        ObservationClassification::StillChangingAtDeadline
    }
}

fn terminal_observation_value(record: &TerminalRecord) -> Value {
    json!({
        "state": record.state,
        "terminal_id": record.id,
        "observation": record.latest_observation,
        "screen": record.latest_snapshot,
        "recent_transcript": record.recent_transcript,
    })
}

fn terminal_number(id: &str) -> u64 {
    id.strip_prefix("terminal_")
        .and_then(|value| value.parse().ok())
        .unwrap_or(u64::MAX)
}

fn next_terminal_id(records: &[TerminalRecord]) -> u64 {
    records
        .iter()
        .map(|record| terminal_number(&record.id))
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1)
}

struct ShellSpec {
    program: PathBuf,
    kind: ShellKind,
}

#[derive(Clone, Copy)]
enum ShellKind {
    PowerShell,
    Cmd,
    Unix,
}

impl ShellSpec {
    fn command(&self, command: &str) -> CommandBuilder {
        let mut builder = CommandBuilder::new(&self.program);
        match self.kind {
            ShellKind::PowerShell => builder.args(["-NoLogo", "-Command", command]),
            ShellKind::Cmd => builder.args(["/D", "/S", "/C", command]),
            ShellKind::Unix => builder.args(["-lic", command]),
        }
        builder
    }
}

fn detect_shell(configured: Option<&str>) -> Result<ShellSpec> {
    if let Some(configured) = configured {
        let program = PathBuf::from(configured);
        return Ok(ShellSpec {
            kind: shell_kind(&program),
            program,
        });
    }
    platform_shell()
}

fn shell_kind(program: &Path) -> ShellKind {
    let name = program
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(name.as_str(), "pwsh" | "powershell") {
        ShellKind::PowerShell
    } else if matches!(name.as_str(), "cmd" | "cmd.exe") {
        ShellKind::Cmd
    } else {
        ShellKind::Unix
    }
}

#[cfg(windows)]
fn platform_shell() -> Result<ShellSpec> {
    if let Some(parent) = parent_shell_name()
        && let Some(program) = find_executable(&parent)
    {
        return Ok(ShellSpec {
            kind: shell_kind(&program),
            program,
        });
    }
    for candidate in ["pwsh.exe", "powershell.exe"] {
        if let Some(program) = find_executable(candidate) {
            return Ok(ShellSpec {
                kind: shell_kind(&program),
                program,
            });
        }
    }
    let program = std::env::var_os("ComSpec")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .context("cannot find pwsh, Windows PowerShell, or ComSpec")?;
    Ok(ShellSpec {
        kind: ShellKind::Cmd,
        program,
    })
}

#[cfg(unix)]
fn platform_shell() -> Result<ShellSpec> {
    use std::ffi::CStr;

    let program = std::env::var_os("SHELL")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| unsafe {
            let entry = libc::getpwuid(libc::getuid());
            (!entry.is_null() && !(*entry).pw_shell.is_null()).then(|| {
                PathBuf::from(
                    CStr::from_ptr((*entry).pw_shell)
                        .to_string_lossy()
                        .into_owned(),
                )
            })
        })
        .unwrap_or_else(|| PathBuf::from("/bin/sh"));
    Ok(ShellSpec {
        kind: ShellKind::Unix,
        program,
    })
}

#[cfg(windows)]
fn find_executable(name: &str) -> Option<PathBuf> {
    let requested = Path::new(name);
    if requested.components().count() > 1 && requested.is_file() {
        return Some(requested.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(windows)]
fn parent_shell_name() -> Option<String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Threading::GetCurrentProcessId,
        },
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let current = GetCurrentProcessId();
        let mut parent = None;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == current {
                    parent = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        let mut result = None;
        if let Some(parent) = parent {
            entry = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snapshot, &mut entry) != 0 {
                loop {
                    if entry.th32ProcessID == parent {
                        let length = entry
                            .szExeFile
                            .iter()
                            .position(|character| *character == 0)
                            .unwrap_or(entry.szExeFile.len());
                        let name = String::from_utf16_lossy(&entry.szExeFile[..length]);
                        let stem = Path::new(&name)
                            .file_stem()
                            .and_then(OsStr::to_str)
                            .unwrap_or_default()
                            .to_ascii_lowercase();
                        if matches!(stem.as_str(), "pwsh" | "powershell" | "cmd") {
                            result = Some(name);
                        }
                        break;
                    }
                    if Process32NextW(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
        }
        CloseHandle(snapshot);
        result
    }
}

#[cfg(unix)]
struct ProcessTree {
    process_group: Option<libc::pid_t>,
    session_id: Option<libc::pid_t>,
}

#[cfg(unix)]
impl ProcessTree {
    fn new(master: &dyn MasterPty, child: &dyn Child) -> Self {
        let session_id = child.process_id().and_then(|pid| {
            let session_id = unsafe { libc::getsid(pid as libc::pid_t) };
            (session_id >= 0).then_some(session_id)
        });
        Self {
            process_group: master.process_group_leader(),
            session_id,
        }
    }

    fn terminate(&self) {
        if let Some(session_id) = self.session_id
            && let Some(mut processes) = session_processes(session_id)
            && !processes.is_empty()
        {
            for process in &processes {
                unsafe {
                    libc::kill(*process, libc::SIGSTOP);
                }
            }
            if let Some(late_processes) = session_processes(session_id) {
                for process in late_processes {
                    if !processes.contains(&process) {
                        unsafe {
                            libc::kill(process, libc::SIGSTOP);
                        }
                        processes.push(process);
                    }
                }
            }
            for process in processes.into_iter().rev() {
                unsafe {
                    libc::kill(process, libc::SIGKILL);
                }
            }
            return;
        }
        if let Some(process_group) = self.process_group {
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
fn session_processes(session_id: libc::pid_t) -> Option<Vec<libc::pid_t>> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,sid="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let current = std::process::id() as libc::pid_t;
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?.parse::<libc::pid_t>().ok()?;
                let sid = fields.next()?.parse::<libc::pid_t>().ok()?;
                (sid == session_id && pid != current).then_some(pid)
            })
            .collect(),
    )
}

#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(windows)]
struct ProcessTree {
    job: usize,
}

#[cfg(windows)]
impl ProcessTree {
    fn new(_master: &dyn MasterPty, child: &dyn Child) -> Self {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Self { job: 0 };
            }
            let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &information as *const _ as *const _,
                std::mem::size_of_val(&information) as u32,
            ) == 0
            {
                windows_sys::Win32::Foundation::CloseHandle(job);
                return Self { job: 0 };
            }
            if let Some(process) = child.as_raw_handle()
                && AssignProcessToJobObject(job, process) == 0
            {
                windows_sys::Win32::Foundation::CloseHandle(job);
                return Self { job: 0 };
            }
            Self { job: job as usize }
        }
    }

    fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        if self.job != 0 {
            unsafe {
                TerminateJobObject(self.job as _, 1);
            }
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        if self.job != 0 {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.job as _);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn test_manager(root: &Path) -> TerminalManager {
        #[cfg(windows)]
        let shell = Some("powershell.exe".into());
        #[cfg(unix)]
        let shell = Some("/bin/sh".into());
        TerminalManager::new(root.to_path_buf(), shell)
    }

    fn fast_command() -> &'static str {
        if cfg!(windows) {
            "Write-Output 'before'; Write-Output 'after'; exit 7"
        } else {
            "printf 'before\\nafter\\n'; exit 7"
        }
    }

    fn prompt_command() -> &'static str {
        if cfg!(windows) {
            "$name = Read-Host 'Name'; Write-Output \"Hello $name\""
        } else {
            "printf 'Name: '; IFS= read name; printf 'Hello %s\\n' \"$name\""
        }
    }

    fn descendant_command() -> &'static str {
        if cfg!(windows) {
            "$p = Start-Process powershell.exe -ArgumentList '-NoLogo','-NoProfile','-Command','Start-Sleep -Seconds 30' -PassThru; Set-Content -LiteralPath child.pid -Value $p.Id; Wait-Process -Id $p.Id"
        } else {
            "sleep 30 & echo $! > child.pid; wait"
        }
    }

    #[cfg(unix)]
    fn process_is_running(pid: u32) -> bool {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if result != 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::PermissionDenied
        {
            return false;
        }
        #[cfg(target_os = "linux")]
        if std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.split_whitespace().nth(2).map(str::to_owned))
            .as_deref()
            == Some("Z")
        {
            return false;
        }
        true
    }

    #[cfg(windows)]
    fn process_is_running(pid: u32) -> bool {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, STILL_ACTIVE},
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };

        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return false;
            }
            let mut exit_code = 0;
            let running = GetExitCodeProcess(process, &mut exit_code) != 0
                && exit_code == STILL_ACTIVE as u32;
            CloseHandle(process);
            running
        }
    }

    #[test]
    fn transcript_reflects_cursor_rewrites_instead_of_raw_ansi() {
        let mut terminal = TerminalEmulator::new(20, 4, Box::new(Vec::<u8>::new()));
        terminal.advance_bytes(b"working\rfinished\x1b[K\r\n");

        let text = transcript(&terminal);

        assert_eq!(text, "finished");
        assert!(!text.contains('\x1b'));
    }

    #[tokio::test]
    async fn restored_running_terminals_become_interrupted_inspectable_and_continue_the_counter() {
        let root = tempfile::tempdir().unwrap();
        let manager = TerminalManager::new(root.path().to_path_buf(), None);
        let now = Utc::now();
        manager.restore(
            &[TerminalRecord {
                id: "terminal_7".into(),
                command: "wait".into(),
                shell: "shell".into(),
                working_directory: root.path().to_path_buf(),
                created_at: now,
                updated_at: now,
                completed_at: None,
                columns: 120,
                rows: 40,
                state: TerminalProcessState::Running,
                exit_code: None,
                latest_snapshot: None,
                latest_observation: ObservationClassification::Unchanged,
                recent_transcript: "waiting".into(),
            }],
            1,
        );

        let (next_id, records) = manager.persisted_state();

        assert_eq!(next_id, 8);
        assert_eq!(records[0].state, TerminalProcessState::Interrupted);
        let inspected = manager.read("terminal_7").await.unwrap();
        assert_eq!(inspected["state"], "interrupted");
        assert_eq!(inspected["recent_transcript"], "waiting");
    }

    #[test]
    fn semantic_keys_and_modifiers_are_validated() {
        assert_eq!(parse_key("ArrowUp").unwrap(), KeyCode::UpArrow);
        assert_eq!(parse_key("Character:ñ").unwrap(), KeyCode::Char('ñ'));
        assert!(parse_key("Character:ab").is_err());
        assert_eq!(
            parse_modifiers(Some(&json!(["Ctrl", "Shift"]))).unwrap(),
            KeyModifiers::CTRL | KeyModifiers::SHIFT
        );
    }

    #[test]
    fn observation_classifications_cover_unchanged_stable_and_deadline_states() {
        assert_eq!(
            classify_observation(false, std::time::Duration::ZERO, false),
            ObservationClassification::Unchanged
        );
        assert_eq!(
            classify_observation(true, SCREEN_STABILITY, false),
            ObservationClassification::ChangedThenStabilized
        );
        assert_eq!(
            classify_observation(true, SCREEN_STABILITY / 2, false),
            ObservationClassification::StillChangingAtDeadline
        );
        assert_eq!(
            classify_observation(true, std::time::Duration::ZERO, true),
            ObservationClassification::ChangedThenStabilized
        );
    }

    #[test]
    fn snapshots_preserve_styles_alt_screen_cursor_and_negotiated_modes() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut terminal = TerminalEmulator::new(20, 4, Box::new(SharedWriter(output.clone())));
        terminal
            .advance_bytes(b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[31;1mred");
        let core = ActorCore {
            emulator: terminal,
            sequence: 3,
            last_change: Instant::now(),
            reader_eof: false,
            closed: false,
        };

        let snapshot = snapshot(
            "terminal_1",
            "demo",
            &core,
            TerminalProcessState::Running,
            None,
            ObservationClassification::ChangedThenStabilized,
            "red".into(),
        );

        assert!(snapshot.alternate_screen);
        assert!(!snapshot.cursor.visible);
        assert!(snapshot.mouse_reporting);
        assert!(snapshot.bracketed_paste);
        assert_eq!(snapshot.lines[0].spans[0].text, "red");
        assert!(snapshot.lines[0].spans[0].style.bold);
        assert!(
            snapshot.lines[0].spans[0]
                .style
                .foreground
                .contains("PaletteIndex(1)")
        );

        let mut terminal = core.emulator;
        terminal.send_paste("safe").unwrap();
        assert!(terminal.is_mouse_grabbed());
        terminal
            .mouse_event(MouseEvent {
                kind: MouseEventKind::Press,
                x: 1,
                y: 2,
                x_pixel_offset: 0,
                y_pixel_offset: 0,
                button: MouseButton::Left,
                modifiers: KeyModifiers::NONE,
            })
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        let encoded = loop {
            let encoded = String::from_utf8(output.lock().unwrap().clone()).unwrap();
            if (encoded.contains("\x1b[200~safe\x1b[201~") && encoded.contains("\x1b[<0;2;3M"))
                || Instant::now() >= deadline
            {
                break encoded;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(
            encoded.contains("\x1b[200~safe\x1b[201~"),
            "encoded terminal input was {encoded:?}"
        );
        assert!(
            encoded.contains("\x1b[<0;2;3M"),
            "encoded terminal input was {encoded:?}"
        );
    }

    #[tokio::test]
    async fn fast_commands_return_normalized_terminal_text_and_exit_status() {
        let root = tempfile::tempdir().unwrap();
        let manager = test_manager(root.path());

        let result = manager.shell(fast_command()).await.unwrap();

        assert_eq!(result["state"], "exited");
        assert_eq!(result["exit_code"], 7);
        assert!(result["text"].as_str().unwrap().contains("before"));
        assert!(result["text"].as_str().unwrap().contains("after"));
        assert!(!result["text"].as_str().unwrap().contains('\x1b'));
    }

    #[tokio::test]
    async fn a_running_terminal_accepts_resize_text_and_semantic_keys() {
        let root = tempfile::tempdir().unwrap();
        let manager = test_manager(root.path());
        let started = manager.shell(prompt_command()).await.unwrap();
        let id = started["terminal_id"].as_str().unwrap();

        let resized = manager
            .input(id, &[json!({"type": "resize", "columns": 90, "rows": 30})])
            .await
            .unwrap();
        assert_eq!(resized["screen"]["columns"], 90);
        assert_eq!(resized["screen"]["rows"], 30);

        let result = manager
            .input(
                id,
                &[
                    json!({"type": "text", "text": "Ada"}),
                    json!({"type": "key", "key": "Enter"}),
                ],
            )
            .await
            .unwrap();
        assert_eq!(result["state"], "exited");
        assert!(
            result["recent_transcript"]
                .as_str()
                .unwrap()
                .contains("Hello Ada")
        );
    }

    #[tokio::test]
    async fn cancelling_initial_observation_removes_and_closes_the_unpublished_terminal() {
        let root = tempfile::tempdir().unwrap();
        let manager = test_manager(root.path());
        let task_manager = manager.clone();
        let command = prompt_command().to_owned();
        let task = tokio::spawn(async move { task_manager.shell(&command).await });
        sleep(std::time::Duration::from_millis(100)).await;

        task.abort();
        let _ = task.await;

        assert_eq!(manager.list()["terminals"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn multiple_terminals_remain_independently_addressable_and_close_on_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let manager = test_manager(root.path());
        let first_manager = manager.clone();
        let second_manager = manager.clone();
        let (first, second) = tokio::join!(
            first_manager.shell(prompt_command()),
            second_manager.shell(prompt_command())
        );
        let first = first.unwrap();
        let second = second.unwrap();
        let first_id = first["terminal_id"].as_str().unwrap();
        let second_id = second["terminal_id"].as_str().unwrap();
        assert_ne!(first_id, second_id);

        manager.close_all().unwrap();
        let terminals = manager.list()["terminals"].as_array().unwrap().clone();
        assert_eq!(terminals.len(), 2);
        assert!(
            terminals
                .iter()
                .all(|terminal| terminal["state"] == "closed")
        );
    }

    #[tokio::test]
    async fn concurrent_starts_enforce_the_live_terminal_limit() {
        let root = tempfile::tempdir().unwrap();
        let manager = test_manager(root.path());
        let mut starts = tokio::task::JoinSet::new();
        for _ in 0..MAX_TERMINALS_PER_SESSION + 1 {
            let manager = manager.clone();
            starts.spawn(async move { manager.shell(prompt_command()).await });
        }

        let mut started = 0;
        let mut rejected = 0;
        while let Some(result) = starts.join_next().await {
            match result.unwrap() {
                Ok(_) => started += 1,
                Err(error) => {
                    rejected += 1;
                    assert!(error.to_string().contains("at most"));
                }
            }
        }

        assert_eq!(started, MAX_TERMINALS_PER_SESSION);
        assert_eq!(rejected, 1);
        manager.close_all().unwrap();
    }

    #[tokio::test]
    async fn closing_terminals_kills_descendant_processes() {
        let root = tempfile::tempdir().unwrap();
        let manager = test_manager(root.path());
        let started = manager.shell(descendant_command()).await.unwrap();
        assert_eq!(started["state"], "running");
        let pid = std::fs::read_to_string(root.path().join("child.pid"))
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert!(process_is_running(pid));

        manager.close_all().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while process_is_running(pid) && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        assert!(
            !process_is_running(pid),
            "descendant process {pid} survived"
        );
    }

    #[test]
    #[ignore = "requires fzf to be installed; run manually on each release platform"]
    fn fzf_smoke_test() {
        assert!(
            std::process::Command::new("fzf")
                .arg("--version")
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    #[ignore = "requires k9s to be installed; run manually on each release platform"]
    fn k9s_smoke_test() {
        assert!(
            std::process::Command::new("k9s")
                .arg("version")
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    #[ignore = "requires Vim to be installed; run manually on each release platform"]
    fn vim_smoke_test() {
        assert!(
            std::process::Command::new("vim")
                .arg("--version")
                .status()
                .unwrap()
                .success()
        );
    }
}
