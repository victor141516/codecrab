use std::{
    fs,
    io::{self, Stdout},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    agent::Agent,
    events::AgentEvent,
    provider::{Message, ModelCatalogEntry, ModelSelection, Role},
    session::{SessionStore, SessionSummary},
};

const CRAB: Color = Color::Rgb(244, 99, 86);
const AQUA: Color = Color::Rgb(74, 210, 200);
const MUTED: Color = Color::Rgb(125, 135, 150);
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const COMMANDS: &[(&str, &str)] = &[
    ("help", "Open keyboard and command help"),
    ("model", "Choose model, reasoning, and speed"),
    ("models", "Alias for /model"),
    ("skills", "Open the interactive skill picker"),
    ("clear", "Clear the conversation context"),
    ("quit", "Save the session and exit"),
];

struct ApprovalPrompt {
    action: String,
    response: oneshot::Sender<bool>,
}

struct SkillView {
    name: String,
    description: String,
    scope: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletionKind {
    Command,
    Skill,
    File,
    Directory,
}

struct CompletionItem {
    name: String,
    description: String,
    icon: Option<&'static str>,
    kind: CompletionKind,
}

struct CompletionMenu {
    items: Vec<CompletionItem>,
    selected: usize,
    token_start: usize,
    token_end: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModelPickerStep {
    Model,
    Reasoning,
    Speed,
}

struct ModelPicker {
    step: ModelPickerStep,
    selected: usize,
    model_index: usize,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
}

struct App {
    agent: Option<Agent>,
    transcript: Vec<Message>,
    running: Option<JoinHandle<(Agent, Result<String>)>>,
    event_rx: Option<mpsc::UnboundedReceiver<AgentEvent>>,
    approval: Option<ApprovalPrompt>,
    input: String,
    cursor: usize,
    preferred_column: Option<usize>,
    pending_user: Option<String>,
    error: Option<String>,
    scroll: u16,
    max_scroll: u16,
    auto_scroll: bool,
    spinner: usize,
    show_help: bool,
    show_skills: bool,
    skill_selection: usize,
    completion: Option<CompletionMenu>,
    model_catalog: Vec<ModelCatalogEntry>,
    model_picker: Option<ModelPicker>,
    should_quit: bool,
    project: String,
    project_root: PathBuf,
    model: String,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    skills: Vec<SkillView>,
}

impl App {
    fn new(
        agent: Agent,
        model_catalog: Vec<ModelCatalogEntry>,
        catalog_error: Option<String>,
    ) -> Self {
        let project_root = agent.project_root().to_path_buf();
        let project = project_root.display().to_string();
        let model = agent.session().model.clone();
        let reasoning_effort = agent.session().reasoning_effort.clone();
        let service_tier = agent.session().service_tier.clone();
        let skills = agent
            .skills()
            .iter()
            .map(|skill| SkillView {
                name: skill.name.clone(),
                description: skill.description.clone(),
                scope: skill.scope.label(),
            })
            .collect::<Vec<_>>();
        let transcript = agent.session().messages.clone();
        let initial_error = catalog_error
            .as_ref()
            .map(|error| format!("Could not load the model catalog: {error}"));
        Self {
            agent: Some(agent),
            transcript,
            running: None,
            event_rx: None,
            approval: None,
            input: String::new(),
            cursor: 0,
            preferred_column: None,
            pending_user: None,
            error: initial_error,
            scroll: 0,
            max_scroll: 0,
            auto_scroll: true,
            spinner: 0,
            show_help: false,
            show_skills: false,
            skill_selection: 0,
            completion: None,
            model_catalog,
            model_picker: None,
            should_quit: false,
            project,
            project_root,
            model,
            reasoning_effort,
            service_tier,
            skills,
        }
    }

    fn is_running(&self) -> bool {
        self.running.is_some()
    }

    fn status(&self) -> String {
        if self.is_running() {
            format!("{} working", SPINNER[self.spinner % SPINNER.len()])
        } else {
            "● ready".into()
        }
    }

    fn uses_fast_service_tier(&self) -> bool {
        let Some(selected) = self.service_tier.as_deref() else {
            return false;
        };
        self.model_catalog
            .iter()
            .find(|model| model.slug == self.model)
            .and_then(|model| model.service_tiers.iter().find(|tier| tier.id == selected))
            .is_some_and(|tier| tier.name.eq_ignore_ascii_case("fast"))
    }

    fn insert(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.preferred_column = None;
        self.refresh_completion();
    }

    fn insert_char(&mut self, value: char) {
        self.input.insert(self.cursor, value);
        self.cursor += value.len_utf8();
        self.preferred_column = None;
        self.refresh_completion();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = self.input[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.input.drain(previous..self.cursor);
        self.cursor = previous;
        self.preferred_column = None;
        self.refresh_completion();
    }

    fn delete(&mut self) {
        if self.cursor == self.input.len() {
            return;
        }
        let next = self.input[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.input.len());
        self.input.drain(self.cursor..next);
        self.preferred_column = None;
        self.refresh_completion();
    }

    fn move_left(&mut self) {
        self.cursor = self.input[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.preferred_column = None;
        self.refresh_completion();
    }

    fn move_right(&mut self) {
        if self.cursor < self.input.len() {
            self.cursor = self.input[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(index, _)| self.cursor + index)
                .unwrap_or(self.input.len());
        }
        self.preferred_column = None;
        self.refresh_completion();
    }

    fn move_vertical(&mut self, delta: isize) -> bool {
        let starts = line_starts(&self.input);
        let current = starts
            .partition_point(|start| *start <= self.cursor)
            .saturating_sub(1);
        let target = current as isize + delta;
        if target < 0 || target >= starts.len() as isize {
            return false;
        }
        let column = self
            .preferred_column
            .unwrap_or_else(|| cursor_line_column(&self.input, self.cursor).1);
        self.preferred_column = Some(column);
        let start = starts[target as usize];
        let end = self.input[start..]
            .find('\n')
            .map(|offset| start + offset)
            .unwrap_or(self.input.len());
        self.cursor = byte_index_at_char_column(&self.input, start, end, column);
        self.refresh_completion();
        true
    }

    fn refresh_completion(&mut self) {
        let previous = self.completion.as_ref().and_then(|menu| {
            menu.items
                .get(menu.selected)
                .map(|item| (item.kind, item.name.clone()))
        });
        if let Some(context) = file_completion_context(&self.input, self.cursor, &self.project_root)
        {
            let items = file_completion_items(&context);
            if items.is_empty() {
                self.completion = None;
                return;
            }
            let selected = previous
                .and_then(|key| {
                    items
                        .iter()
                        .position(|item| (item.kind, item.name.clone()) == key)
                })
                .unwrap_or(0);
            self.completion = Some(CompletionMenu {
                items,
                selected,
                token_start: context.start,
                token_end: context.end,
            });
            return;
        }

        let Some(context) = slash_completion_context(&self.input, self.cursor) else {
            self.completion = None;
            return;
        };

        let mut items = Vec::new();
        if context.commands_allowed {
            items.extend(
                COMMANDS
                    .iter()
                    .filter(|(name, _)| name.starts_with(context.prefix))
                    .map(|(name, description)| CompletionItem {
                        name: (*name).to_owned(),
                        description: (*description).to_owned(),
                        icon: None,
                        kind: CompletionKind::Command,
                    }),
            );
        }
        items.extend(
            self.skills
                .iter()
                .filter(|skill| skill.name.starts_with(context.prefix))
                .map(|skill| CompletionItem {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    icon: None,
                    kind: CompletionKind::Skill,
                }),
        );
        if items.is_empty() {
            self.completion = None;
            return;
        }
        let selected = previous
            .and_then(|key| {
                items
                    .iter()
                    .position(|item| (item.kind, item.name.clone()) == key)
            })
            .unwrap_or(0);
        self.completion = Some(CompletionMenu {
            items,
            selected,
            token_start: context.start,
            token_end: context.end,
        });
    }

    fn move_completion(&mut self, delta: isize) {
        let Some(menu) = &mut self.completion else {
            return;
        };
        let len = menu.items.len() as isize;
        menu.selected = (menu.selected as isize + delta).rem_euclid(len) as usize;
    }

    fn accept_completion(&mut self) -> bool {
        let Some(menu) = self.completion.take() else {
            return false;
        };
        let Some(item) = menu.items.get(menu.selected) else {
            return false;
        };
        let replacement = match item.kind {
            CompletionKind::Command | CompletionKind::Skill => format!("/{}", item.name),
            CompletionKind::File | CompletionKind::Directory => format!("@{}", item.name),
        };
        self.input
            .replace_range(menu.token_start..menu.token_end, &replacement);
        self.cursor = menu.token_start + replacement.len();
        match item.kind {
            CompletionKind::Skill | CompletionKind::File => {
                self.input.insert(self.cursor, ' ');
                self.cursor += 1;
            }
            CompletionKind::Directory => {
                self.input.insert(self.cursor, '/');
                self.cursor += 1;
                self.refresh_completion();
                return true;
            }
            CompletionKind::Command => {}
        }
        self.preferred_column = None;
        true
    }

    fn open_skill_picker(&mut self) {
        self.show_skills = true;
        self.skill_selection = self
            .skill_selection
            .min(self.skills.len().saturating_sub(1));
        self.completion = None;
    }

    fn move_skill_selection(&mut self, delta: isize) {
        if self.skills.is_empty() {
            return;
        }
        let len = self.skills.len() as isize;
        self.skill_selection = (self.skill_selection as isize + delta).rem_euclid(len) as usize;
    }

    fn accept_skill_selection(&mut self) {
        let Some(skill) = self.skills.get(self.skill_selection) else {
            self.show_skills = false;
            return;
        };
        let name = skill.name.clone();
        let needs_leading_space = self.cursor > 0
            && !self.input[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let mut mention = String::new();
        if needs_leading_space {
            mention.push(' ');
        }
        mention.push('/');
        mention.push_str(&name);
        mention.push(' ');
        self.show_skills = false;
        self.insert(&mention);
        self.completion = None;
    }

    fn open_model_picker(&mut self) {
        if self.model_catalog.is_empty() {
            self.error = Some("The provider did not return a selectable model catalog.".into());
            return;
        }
        let selected = self
            .model_catalog
            .iter()
            .position(|model| model.slug == self.model)
            .unwrap_or(0);
        self.model_picker = Some(ModelPicker {
            step: ModelPickerStep::Model,
            selected,
            model_index: selected,
            reasoning_effort: self.reasoning_effort.clone(),
            service_tier: self.service_tier.clone(),
        });
        self.completion = None;
    }

    fn model_picker_item_count(&self) -> usize {
        let Some(picker) = &self.model_picker else {
            return 0;
        };
        match picker.step {
            ModelPickerStep::Model => self.model_catalog.len(),
            ModelPickerStep::Reasoning => self
                .model_catalog
                .get(picker.model_index)
                .map(|model| model.supported_reasoning_levels.len())
                .unwrap_or(0),
            ModelPickerStep::Speed => self
                .model_catalog
                .get(picker.model_index)
                .map(|model| model.available_service_tiers().len() + 1)
                .unwrap_or(0),
        }
    }

    fn move_model_selection(&mut self, delta: isize) {
        let len = self.model_picker_item_count();
        if len == 0 {
            return;
        }
        if let Some(picker) = &mut self.model_picker {
            picker.selected = (picker.selected as isize + delta).rem_euclid(len as isize) as usize;
        }
    }

    fn back_model_picker(&mut self) {
        let Some(picker) = &mut self.model_picker else {
            return;
        };
        match picker.step {
            ModelPickerStep::Model => self.model_picker = None,
            ModelPickerStep::Reasoning => {
                picker.step = ModelPickerStep::Model;
                picker.selected = picker.model_index;
            }
            ModelPickerStep::Speed => {
                let has_reasoning = self
                    .model_catalog
                    .get(picker.model_index)
                    .is_some_and(|model| !model.supported_reasoning_levels.is_empty());
                if has_reasoning {
                    picker.step = ModelPickerStep::Reasoning;
                    picker.selected = self
                        .model_catalog
                        .get(picker.model_index)
                        .and_then(|model| {
                            model.supported_reasoning_levels.iter().position(|option| {
                                Some(option.effort.as_str()) == picker.reasoning_effort.as_deref()
                            })
                        })
                        .unwrap_or(0);
                } else {
                    picker.step = ModelPickerStep::Model;
                    picker.selected = picker.model_index;
                }
            }
        }
    }

    fn accept_model_selection(&mut self, store: &SessionStore) -> Result<()> {
        let Some(picker) = &mut self.model_picker else {
            return Ok(());
        };
        match picker.step {
            ModelPickerStep::Model => {
                picker.model_index = picker.selected;
                let Some(model) = self.model_catalog.get(picker.model_index) else {
                    return Ok(());
                };
                let keep_current_reasoning = model.slug == self.model
                    && picker.reasoning_effort.as_deref().is_some_and(|effort| {
                        model
                            .supported_reasoning_levels
                            .iter()
                            .any(|option| option.effort == effort)
                    });
                if !keep_current_reasoning {
                    picker.reasoning_effort = model.default_reasoning_level.clone();
                }
                picker.service_tier = if model.slug == self.model {
                    self.service_tier.clone()
                } else {
                    model
                        .default_service_tier
                        .clone()
                        .filter(|tier| tier != "default")
                };
                if model.supported_reasoning_levels.is_empty() {
                    picker.step = ModelPickerStep::Speed;
                    picker.selected = speed_tier_index(model, picker.service_tier.as_deref());
                } else {
                    picker.step = ModelPickerStep::Reasoning;
                    picker.selected = model
                        .supported_reasoning_levels
                        .iter()
                        .position(|option| {
                            Some(option.effort.as_str()) == picker.reasoning_effort.as_deref()
                        })
                        .unwrap_or(0);
                }
            }
            ModelPickerStep::Reasoning => {
                let Some(model) = self.model_catalog.get(picker.model_index) else {
                    return Ok(());
                };
                picker.reasoning_effort = model
                    .supported_reasoning_levels
                    .get(picker.selected)
                    .map(|option| option.effort.clone());
                picker.step = ModelPickerStep::Speed;
                picker.selected = speed_tier_index(model, picker.service_tier.as_deref());
            }
            ModelPickerStep::Speed => {
                let Some(model) = self.model_catalog.get(picker.model_index) else {
                    return Ok(());
                };
                let tiers = model.available_service_tiers();
                let service_tier = picker
                    .selected
                    .checked_sub(1)
                    .and_then(|index| tiers.get(index))
                    .map(|tier| tier.id.clone());
                let selection = ModelSelection {
                    model: model.slug.clone(),
                    reasoning_effort: picker.reasoning_effort.clone(),
                    service_tier,
                };
                if let Some(agent) = &mut self.agent {
                    agent.set_model_selection(selection.clone());
                    store.save(agent.session())?;
                }
                self.model.clone_from(&selection.model);
                self.reasoning_effort
                    .clone_from(&selection.reasoning_effort);
                self.service_tier.clone_from(&selection.service_tier);
                self.error = None;
                self.model_picker = None;
            }
        }
        Ok(())
    }

    fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
        self.auto_scroll = false;
    }

    fn scroll_down(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_add(amount).min(self.max_scroll);
        self.auto_scroll = self.scroll >= self.max_scroll;
    }

    async fn drain_agent_events(&mut self) {
        let mut pending = Vec::new();
        if let Some(receiver) = &mut self.event_rx {
            while let Ok(event) = receiver.try_recv() {
                pending.push(event);
            }
        }
        for event in pending {
            match event {
                AgentEvent::ApprovalRequested { action, response } => {
                    self.approval = Some(ApprovalPrompt { action, response });
                }
            }
        }
    }

    async fn finish_turn_if_ready(&mut self, store: &SessionStore) -> Result<()> {
        if !self.running.as_ref().is_some_and(JoinHandle::is_finished) {
            return Ok(());
        }
        let handle = self.running.take().expect("checked above");
        let (agent, result) = handle.await.context("agent task failed")?;
        self.transcript = agent.session().messages.clone();
        store.save(agent.session())?;
        self.agent = Some(agent);
        self.event_rx = None;
        self.pending_user = None;
        self.approval = None;
        self.auto_scroll = true;
        match result {
            Ok(_) => {
                self.error = None;
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
            }
        }
        Ok(())
    }

    fn submit(&mut self, store: &SessionStore) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }
        let prompt = self.input.trim().to_owned();
        if prompt.is_empty() {
            return Ok(());
        }
        if builtin_command_from_input(&self.input).is_some() {
            self.input.clear();
            self.cursor = 0;
            self.preferred_column = None;
            self.completion = None;
            return self.command(&prompt, store);
        }

        self.input.clear();
        self.cursor = 0;
        self.preferred_column = None;
        self.completion = None;
        self.error = None;
        self.pending_user = Some(prompt.clone());
        self.auto_scroll = true;

        let mut agent = self.agent.take().context("agent is unavailable")?;
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_rx = Some(event_rx);
        self.running = Some(tokio::spawn(async move {
            let result = agent.turn_with_events(&prompt, event_tx).await;
            (agent, result)
        }));
        Ok(())
    }

    fn command(&mut self, command: &str, store: &SessionStore) -> Result<()> {
        match command {
            "/quit" => self.should_quit = true,
            "/help" => self.show_help = true,
            "/model" | "/models" => self.open_model_picker(),
            "/skills" => self.open_skill_picker(),
            "/clear" => {
                if let Some(agent) = &mut self.agent {
                    agent.clear();
                    store.save(agent.session())?;
                    self.transcript.clear();
                    self.error = None;
                }
            }
            _ => self.error = Some(format!("Unknown command: {command}")),
        }
        Ok(())
    }

    fn handle_approval_key(&mut self, key: KeyEvent) -> bool {
        if self.approval.is_none() {
            return false;
        }
        let answer = match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(true),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
            _ => None,
        };
        if let Some(answer) = answer
            && let Some(prompt) = self.approval.take()
        {
            let _ = prompt.response.send(answer);
        }
        true
    }

    fn handle_key(&mut self, key: KeyEvent, store: &SessionStore) -> Result<()> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(());
        }
        if self.handle_approval_key(key) {
            return Ok(());
        }
        if self.show_help {
            if matches!(key.code, KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?')) {
                self.show_help = false;
            }
            return Ok(());
        }
        if self.show_skills {
            match key.code {
                KeyCode::Up => self.move_skill_selection(-1),
                KeyCode::Down => self.move_skill_selection(1),
                KeyCode::PageUp => self.move_skill_selection(-5),
                KeyCode::PageDown => self.move_skill_selection(5),
                KeyCode::Enter | KeyCode::Tab => self.accept_skill_selection(),
                KeyCode::Esc | KeyCode::F(2) => self.show_skills = false,
                _ => {}
            }
            return Ok(());
        }
        if self.model_picker.is_some() {
            match key.code {
                KeyCode::Up => self.move_model_selection(-1),
                KeyCode::Down => self.move_model_selection(1),
                KeyCode::PageUp => self.move_model_selection(-5),
                KeyCode::PageDown => self.move_model_selection(5),
                KeyCode::Enter | KeyCode::Tab => self.accept_model_selection(store)?,
                KeyCode::Left | KeyCode::Backspace | KeyCode::Esc => self.back_model_picker(),
                _ => {}
            }
            return Ok(());
        }

        if self.completion.is_some()
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            match key.code {
                KeyCode::Up => {
                    self.move_completion(-1);
                    return Ok(());
                }
                KeyCode::Down => {
                    self.move_completion(1);
                    return Ok(());
                }
                KeyCode::PageUp => {
                    self.move_completion(-5);
                    return Ok(());
                }
                KeyCode::PageDown => {
                    self.move_completion(5);
                    return Ok(());
                }
                KeyCode::Tab => {
                    self.accept_completion();
                    return Ok(());
                }
                KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.accept_completion();
                    return Ok(());
                }
                KeyCode::Esc => {
                    self.completion = None;
                    return Ok(());
                }
                _ => {}
            }
        }

        if key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Char('c' | 'd') if !self.is_running() => self.should_quit = true,
                KeyCode::Char('c') => {}
                KeyCode::Char('j') => self.insert("\n"),
                KeyCode::Char('u') => {
                    self.input.clear();
                    self.cursor = 0;
                    self.preferred_column = None;
                    self.completion = None;
                }
                KeyCode::Char('a') => {
                    self.cursor = 0;
                    self.preferred_column = None;
                    self.refresh_completion();
                }
                KeyCode::Char('e') => {
                    self.cursor = self.input.len();
                    self.preferred_column = None;
                    self.refresh_completion();
                }
                _ => {}
            }
            return Ok(());
        }
        // On Windows, AltGr is reported as Ctrl+Alt while `KeyCode::Char` already
        // contains the character resolved through the active keyboard layout.
        // Unknown Ctrl-only chords are controls, not text; Ctrl+Alt characters
        // continue into the normal text-input branch below.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            return Ok(());
        }

        match key.code {
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert("\n");
            }
            KeyCode::Enter => self.submit(store)?,
            KeyCode::Char('?') if self.input.is_empty() => self.show_help = true,
            KeyCode::Char(value) => self.insert_char(value),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Home => {
                self.cursor = 0;
                self.preferred_column = None;
                self.refresh_completion();
            }
            KeyCode::End => {
                self.cursor = self.input.len();
                self.preferred_column = None;
                self.auto_scroll = true;
                self.refresh_completion();
            }
            KeyCode::Up => {
                if !self.move_vertical(-1) {
                    self.scroll_up(2);
                }
            }
            KeyCode::Down => {
                if !self.move_vertical(1) {
                    self.scroll_down(2);
                }
            }
            KeyCode::PageUp => self.scroll_up(10),
            KeyCode::PageDown => self.scroll_down(10),
            KeyCode::F(1) => self.show_help = true,
            KeyCode::F(2) => self.open_skill_picker(),
            KeyCode::Esc => {
                self.input.clear();
                self.cursor = 0;
                self.preferred_column = None;
                self.completion = None;
            }
            _ => {}
        }
        Ok(())
    }
}

pub(crate) async fn interactive(mut agent: Agent, store: &SessionStore) -> Result<()> {
    let (model_catalog, catalog_error) = match agent.fetch_models().await {
        Ok(catalog) => {
            agent.resolve_auto_model(&catalog);
            (catalog, None)
        }
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = run_tui(
        &mut terminal,
        App::new(agent, model_catalog, catalog_error),
        store,
    )
    .await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    let session_id = result?;
    println!("\x1b[2mSession saved as {session_id}\x1b[0m");
    Ok(())
}

async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut app: App,
    store: &SessionStore,
) -> Result<String> {
    loop {
        app.drain_agent_events().await;
        app.finish_turn_if_ready(store).await?;
        app.spinner = app.spinner.wrapping_add(1);
        terminal.draw(|frame| render(frame, &mut app))?;

        if app.should_quit && !app.is_running() {
            break;
        }
        if event::poll(Duration::from_millis(70))? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key, store)?,
                Event::Paste(text) => app.insert(&text.replace("\r\n", "\n")),
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp if app.show_skills => {
                        app.move_skill_selection(-1);
                    }
                    MouseEventKind::ScrollDown if app.show_skills => {
                        app.move_skill_selection(1);
                    }
                    MouseEventKind::ScrollUp if app.model_picker.is_some() => {
                        app.move_model_selection(-1);
                    }
                    MouseEventKind::ScrollDown if app.model_picker.is_some() => {
                        app.move_model_selection(1);
                    }
                    MouseEventKind::ScrollUp if app.completion.is_some() => {
                        app.move_completion(-1);
                    }
                    MouseEventKind::ScrollDown if app.completion.is_some() => {
                        app.move_completion(1);
                    }
                    MouseEventKind::ScrollUp => app.scroll_up(3),
                    MouseEventKind::ScrollDown => app.scroll_down(3),
                    _ => {}
                },
                _ => {}
            }
        }
    }

    let agent = app.agent.context("agent did not return before exit")?;
    store.save(agent.session())?;
    Ok(agent.session().id.to_string())
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let input_lines = app.input.lines().count().clamp(1, 6) as u16;
    let [header, body, composer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(input_lines + 2),
    ])
    .areas(area);

    render_header(frame, app, header);
    render_chat(frame, app, body);
    render_composer(frame, app, composer);
    if app.completion.is_some() {
        render_completion(frame, app, body, composer);
    }

    if app.show_help {
        render_help(frame, area);
    }
    if app.show_skills {
        render_skills(frame, app, area);
    }
    if app.model_picker.is_some() {
        render_model_picker(frame, app, area);
    }
    if let Some(approval) = &app.approval {
        render_approval(frame, area, &approval.action);
    }
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CRAB))
        .title(Span::styled(
            " 🦀 CODECRAB ",
            Style::default().fg(CRAB).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let status_color = if app.is_running() {
        Color::Yellow
    } else {
        AQUA
    };
    let status = app.status();
    let thinking = app.reasoning_effort.as_deref().unwrap_or("default");
    let fast = app.uses_fast_service_tier();
    let separator = "  │  ";
    let fixed_width = status.chars().count()
        + app.model.chars().count()
        + thinking.chars().count()
        + separator.chars().count() * if fast { 4 } else { 3 }
        + usize::from(fast);
    let mut spans = vec![
        Span::styled(status, Style::default().fg(status_color)),
        Span::styled("  │  ", Style::default().fg(MUTED)),
        Span::styled(&app.model, Style::default().fg(Color::White)),
        Span::styled("  │  ", Style::default().fg(MUTED)),
        Span::styled(thinking, Style::default().fg(AQUA)),
    ];
    if fast {
        spans.extend([
            Span::styled(separator, Style::default().fg(MUTED)),
            Span::styled("⚡", Style::default().fg(Color::Yellow)),
        ]);
    }
    spans.extend([
        Span::styled(separator, Style::default().fg(MUTED)),
        Span::styled(
            compact_path(
                &app.project,
                inner.width.saturating_sub(fixed_width as u16) as usize,
            ),
            Style::default().fg(MUTED),
        ),
    ]);
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn render_chat(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Conversation ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = conversation_lines(app);
    let width = inner.width.max(1) as usize;
    let content_height: usize = lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum();
    app.max_scroll = content_height
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    if app.auto_scroll {
        app.scroll = app.max_scroll;
    } else {
        app.scroll = app.scroll.min(app.max_scroll);
    }
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((app.scroll, 0));
    frame.render_widget(paragraph, inner);
}

fn conversation_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for message in &app.transcript {
        let (label, color) = match message.role {
            Role::User => ("YOU", AQUA),
            Role::Assistant => ("CRAB", CRAB),
            _ => continue,
        };
        let Some(content) = message
            .content
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        else {
            continue;
        };
        lines.push(Line::from(Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )));
        for line in content.lines() {
            lines.push(Line::from(Span::raw(line.to_owned())));
        }
        lines.push(Line::default());
    }
    if let Some(content) = &app.pending_user {
        lines.push(Line::from(Span::styled(
            " YOU ",
            Style::default()
                .fg(Color::Black)
                .bg(AQUA)
                .add_modifier(Modifier::BOLD),
        )));
        lines.extend(content.lines().map(|line| Line::from(line.to_owned())));
        lines.push(Line::default());
    }
    if let Some(error) = &app.error {
        lines.push(Line::from(Span::styled(
            " ERROR ",
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        )));
        lines.extend(error.lines().map(|line| {
            Line::from(Span::styled(
                line.to_owned(),
                Style::default().fg(Color::Red),
            ))
        }));
    }
    lines
}

fn render_composer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let color = if app.is_running() {
        Color::Yellow
    } else {
        AQUA
    };
    let title = if app.is_running() {
        " Draft · agent working "
    } else {
        " Message "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Span::styled(title, Style::default().fg(color)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (line_index, column) = cursor_line_column(&app.input, app.cursor);
    let all_lines = app.input.split('\n').collect::<Vec<_>>();
    let visible = inner.height.max(1) as usize;
    let start = line_index.saturating_sub(visible - 1);
    let shown = all_lines
        .iter()
        .skip(start)
        .take(visible)
        .map(|line| Line::from((*line).to_owned()))
        .collect::<Vec<_>>();
    if app.input.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Ask about the codebase or describe a change…",
                Style::default().fg(Color::DarkGray),
            )),
            inner,
        );
    } else {
        frame.render_widget(Paragraph::new(shown), inner);
    }
    if app.approval.is_none() && !app.show_help && !app.show_skills && app.model_picker.is_none() {
        frame.set_cursor_position((
            inner.x + column.min(inner.width.saturating_sub(1) as usize) as u16,
            inner.y + line_index.saturating_sub(start) as u16,
        ));
    }
}

fn render_completion(frame: &mut Frame<'_>, app: &App, body: Rect, composer: Rect) {
    let Some(menu) = &app.completion else {
        return;
    };
    let is_file_menu = menu
        .items
        .iter()
        .any(|item| matches!(item.kind, CompletionKind::File | CompletionKind::Directory));
    let desired_rows = menu.items.len().min(8) as u16;
    let height = (desired_rows + 2).min(body.height);
    if height < 3 {
        return;
    }
    let width = composer.width.saturating_sub(2).min(86);
    if width < 20 {
        return;
    }
    let popup = Rect::new(
        composer.x + 1,
        composer.y.saturating_sub(height).max(body.y),
        width,
        height,
    );
    let visible = popup.height.saturating_sub(2) as usize;
    let start = menu
        .selected
        .saturating_sub(visible.saturating_sub(1))
        .min(menu.items.len().saturating_sub(visible));
    let lines = menu
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, item)| {
            let selected = index == menu.selected;
            if is_file_menu {
                let color = if item.kind == CompletionKind::Directory {
                    Color::Yellow
                } else {
                    Color::White
                };
                let name = item
                    .name
                    .rsplit('/')
                    .next()
                    .expect("split always yields at least one component");
                return Line::from(vec![
                    Span::styled(
                        if selected { " › " } else { "   " },
                        Style::default().fg(color),
                    ),
                    Span::styled(
                        format!("{} ", item.icon.expect("file completions have icons")),
                        Style::default().fg(color),
                    ),
                    Span::styled(
                        name.to_owned(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                ])
                .style(if selected {
                    Style::default().bg(Color::Rgb(42, 48, 58))
                } else {
                    Style::default()
                });
            }
            let (prefix, badge, color) = match item.kind {
                CompletionKind::Command => ("/", "command", AQUA),
                CompletionKind::Skill => ("/", "skill", CRAB),
                CompletionKind::File | CompletionKind::Directory => unreachable!(),
            };
            Line::from(vec![
                Span::styled(
                    if selected { " › " } else { "   " },
                    Style::default().fg(color),
                ),
                Span::styled(
                    format!("{prefix}{}", item.name),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {badge:<7}  "), Style::default().fg(MUTED)),
                Span::styled(item.description.clone(), Style::default().fg(Color::White)),
            ])
            .style(if selected {
                Style::default().bg(Color::Rgb(42, 48, 58))
            } else {
                Style::default()
            })
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(AQUA))
                .title(Span::styled(
                    if is_file_menu {
                        " Files "
                    } else {
                        " Slash menu "
                    },
                    Style::default().fg(AQUA).add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );
}

fn render_approval(frame: &mut Frame<'_>, area: Rect, action: &str) {
    let popup = centered_rect(area, 70, 9);
    frame.render_widget(Clear, popup);
    let content = vec![
        Line::from(Span::styled(
            "The agent wants permission to:",
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(
            action.to_owned(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::default(),
        Line::from(vec![
            Span::styled(" Enter / Y ", Style::default().fg(Color::Black).bg(AQUA)),
            Span::raw(" allow    "),
            Span::styled(
                " N / Esc ",
                Style::default().fg(Color::White).bg(Color::Red),
            ),
            Span::raw(" deny"),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(content).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(Span::styled(
                    " Approval required ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(area, 74, 24);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            "Keyboard",
            Style::default().fg(CRAB).add_modifier(Modifier::BOLD),
        )),
        Line::from("  Enter                 complete selection or send"),
        Line::from("  Tab                   complete slash selection"),
        Line::from("  Shift+Enter / Ctrl+J  insert newline"),
        Line::from("  ↑ / ↓                 navigate menu or move between lines"),
        Line::from("  PgUp / PgDn           scroll conversation"),
        Line::from("  Ctrl+U                clear composer"),
        Line::from("  F2                    show available skills"),
        Line::from("  Ctrl+D / Ctrl+C       save and quit"),
        Line::default(),
        Line::from(Span::styled(
            "Slash menu",
            Style::default().fg(CRAB).add_modifier(Modifier::BOLD),
        )),
        Line::from("  / at input start      commands + skills"),
        Line::from("  / after text          skills only"),
        Line::from("  /skill-name           activate a skill in the prompt"),
        Line::from("  @path                  autocomplete files and folders"),
        Line::default(),
        Line::from(Span::styled(
            "Commands",
            Style::default().fg(CRAB).add_modifier(Modifier::BOLD),
        )),
        Line::from("  /clear     clear conversation context"),
        Line::from("  /model     choose model, thinking, and speed"),
        Line::from("  /skills    show available skills"),
        Line::from("  /quit      save and quit"),
        Line::default(),
        Line::from(Span::styled(
            "Press Esc, F1, or ? to close",
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(AQUA))
                .title(Span::styled(
                    " Help ",
                    Style::default().fg(AQUA).add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );
}

fn render_skills(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let height = area.height.saturating_sub(4).clamp(10, 24);
    let popup = centered_rect(area, 78, height);
    frame.render_widget(Clear, popup);
    let available = popup.height.saturating_sub(4) as usize;
    let start = app
        .skill_selection
        .saturating_sub(available.saturating_sub(1))
        .min(app.skills.len().saturating_sub(available));
    let mut lines = vec![Line::from(Span::styled(
        "↑↓ select  •  Enter/Tab insert  •  Esc close",
        Style::default().fg(MUTED),
    ))];
    lines.push(Line::default());
    for (index, skill) in app.skills.iter().enumerate().skip(start).take(available) {
        let selected = index == app.skill_selection;
        lines.push(
            Line::from(vec![
                Span::styled(
                    if selected { " › " } else { "   " },
                    Style::default().fg(CRAB),
                ),
                Span::styled(
                    format!("/{}", skill.name),
                    Style::default().fg(CRAB).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {:<7}  ", skill.scope),
                    Style::default().fg(MUTED),
                ),
                Span::styled(skill.description.clone(), Style::default().fg(Color::White)),
            ])
            .style(if selected {
                Style::default().bg(Color::Rgb(42, 48, 58))
            } else {
                Style::default()
            }),
        );
    }
    if app.skills.is_empty() {
        lines.push(Line::from("No skills found."));
        lines.push(Line::from(Span::styled(
            "Add one at .agents/skills/<name>/SKILL.md",
            Style::default().fg(MUTED),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(CRAB))
                .title(Span::styled(
                    format!(" Skills ({}) ", app.skills.len()),
                    Style::default().fg(CRAB).add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );
}

fn render_model_picker(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(picker) = &app.model_picker else {
        return;
    };
    let Some(model) = app.model_catalog.get(picker.model_index) else {
        return;
    };
    let (stage, items): (&str, Vec<(String, String)>) = match picker.step {
        ModelPickerStep::Model => (
            "1 Model",
            app.model_catalog
                .iter()
                .map(|entry| {
                    (
                        hierarchical_model_label(entry, &app.model_catalog),
                        entry.description.clone().unwrap_or_default(),
                    )
                })
                .collect(),
        ),
        ModelPickerStep::Reasoning => (
            "2 Thinking",
            model
                .supported_reasoning_levels
                .iter()
                .map(|option| (option.effort.clone(), option.description.clone()))
                .collect(),
        ),
        ModelPickerStep::Speed => {
            let mut speeds = vec![(
                "Standard".to_owned(),
                "Normal service tier (no override)".to_owned(),
            )];
            speeds.extend(
                model
                    .available_service_tiers()
                    .into_iter()
                    .map(|tier| (tier.name, tier.description)),
            );
            ("3 Speed", speeds)
        }
    };
    let height = (items.len().min(12) as u16 + 6).clamp(9, area.height.saturating_sub(2).max(9));
    let popup = centered_rect(area, 82, height);
    frame.render_widget(Clear, popup);
    let available = popup.height.saturating_sub(5) as usize;
    let start = picker
        .selected
        .saturating_sub(available.saturating_sub(1))
        .min(items.len().saturating_sub(available));
    let breadcrumb = match picker.step {
        ModelPickerStep::Model => "MODEL  ›  thinking  ›  speed",
        ModelPickerStep::Reasoning => "model  ›  THINKING  ›  speed",
        ModelPickerStep::Speed => "model  ›  thinking  ›  SPEED",
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                breadcrumb,
                Style::default().fg(AQUA).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if picker.step == ModelPickerStep::Model {
                    String::new()
                } else {
                    format!("    {}", model.label())
                },
                Style::default().fg(MUTED),
            ),
        ]),
        Line::from(Span::styled(
            "↑↓ select  •  Enter/Tab continue  •  ←/Esc back",
            Style::default().fg(MUTED),
        )),
        Line::default(),
    ];
    for (index, (label, description)) in items.iter().enumerate().skip(start).take(available) {
        let selected = index == picker.selected;
        lines.push(
            Line::from(vec![
                Span::styled(
                    if selected { " › " } else { "   " },
                    Style::default().fg(CRAB),
                ),
                Span::styled(
                    format!("{label:<20}"),
                    Style::default()
                        .fg(if selected { Color::White } else { AQUA })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(description.clone(), Style::default().fg(MUTED)),
            ])
            .style(if selected {
                Style::default().bg(Color::Rgb(42, 48, 58))
            } else {
                Style::default()
            }),
        );
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(CRAB))
                .title(Span::styled(
                    format!(" Model · {stage} "),
                    Style::default().fg(CRAB).add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );
}

fn centered_rect(area: Rect, percent_x: u16, height: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(area.height.saturating_sub(height) / 2),
            Constraint::Length(height.min(area.height)),
            Constraint::Min(0),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
        .inner(Margin {
            horizontal: 0,
            vertical: 0,
        })
}

struct SlashCompletionContext<'a> {
    start: usize,
    end: usize,
    prefix: &'a str,
    commands_allowed: bool,
}

struct FileCompletionContext {
    start: usize,
    end: usize,
    dir_prefix: String,
    name_prefix: String,
    directory: PathBuf,
}

fn file_completion_context(
    input: &str,
    cursor: usize,
    project_root: &Path,
) -> Option<FileCompletionContext> {
    let before = input.get(..cursor)?;
    let start = before.rfind('@')?;
    if start > 0
        && !input[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        return None;
    }
    let typed = &input[start + 1..cursor];
    if typed.chars().any(char::is_whitespace) {
        return None;
    }
    let normalized = typed.replace('\\', "/");
    let (dir_prefix, name_prefix) = normalized
        .rfind('/')
        .map(|slash| {
            (
                normalized[..=slash].to_owned(),
                normalized[slash + 1..].to_owned(),
            )
        })
        .unwrap_or_else(|| (String::new(), normalized));
    let directory = project_root.join(dir_prefix.replace('/', std::path::MAIN_SEPARATOR_STR));
    let mut end = cursor;
    while end < input.len() {
        let Some(value) = input[end..].chars().next() else {
            break;
        };
        if value.is_whitespace() {
            break;
        }
        end += value.len_utf8();
    }
    Some(FileCompletionContext {
        start,
        end,
        dir_prefix,
        name_prefix,
        directory,
    })
}

fn file_completion_items(context: &FileCompletionContext) -> Vec<CompletionItem> {
    let Ok(entries) = fs::read_dir(&context.directory) else {
        return Vec::new();
    };
    let prefix = context.name_prefix.to_lowercase();
    let mut items = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.to_lowercase().starts_with(&prefix) {
                return None;
            }
            let is_dir = entry.file_type().ok()?.is_dir();
            let kind = if is_dir {
                CompletionKind::Directory
            } else {
                CompletionKind::File
            };
            Some(CompletionItem {
                name: format!("{}{}", context.dir_prefix, name),
                description: String::new(),
                icon: Some(if is_dir {
                    NERD_FOLDER
                } else {
                    file_icon(&entry.path())
                }),
                kind,
            })
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        let left_dir = left.kind == CompletionKind::Directory;
        let right_dir = right.kind == CompletionKind::Directory;
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    items
}

const NERD_FOLDER: &str = "";
const NERD_FILE: &str = "";

fn file_icon(path: &Path) -> &'static str {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match file_name.as_str() {
        "dockerfile" | "containerfile" => return "󰡨",
        "makefile" => return "",
        ".gitignore" | ".gitattributes" | ".gitmodules" => return "",
        "license" | "license.md" | "license.txt" => return "",
        _ => {}
    }
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "js" | "mjs" | "cjs" => "",
        "jsx" => "",
        "ts" => "",
        "tsx" => "",
        "py" | "pyw" => "",
        "rs" => "",
        "go" => "",
        "java" | "jar" => "",
        "c" | "h" => "",
        "cc" | "cpp" | "cxx" | "hpp" => "",
        "cs" => "󰌛",
        "html" | "htm" => "",
        "css" | "scss" | "sass" | "less" => "",
        "vue" => "",
        "svelte" => "",
        "rb" => "",
        "php" => "",
        "swift" => "",
        "kt" | "kts" => "",
        "lua" => "",
        "sh" | "bash" | "zsh" | "fish" | "ps1" => "",
        "json" | "jsonc" => "",
        "toml" => "",
        "yaml" | "yml" => "",
        "xml" => "󰗀",
        "md" | "markdown" | "rst" => "",
        "sql" | "db" | "sqlite" => "",
        "lock" => "",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" => "",
        "zip" | "tar" | "gz" | "7z" | "rar" => "",
        "mp3" | "wav" | "flac" | "ogg" => "",
        "mp4" | "mov" | "mkv" | "webm" => "",
        "pdf" => "",
        "txt" => "󰈙",
        _ => NERD_FILE,
    }
}

fn slash_completion_context(input: &str, cursor: usize) -> Option<SlashCompletionContext<'_>> {
    let before = input.get(..cursor)?;
    let start = before.rfind('/')?;
    if start > 0 && input.as_bytes()[start - 1] == b'/' {
        return None;
    }
    let prefix = &input[start + 1..cursor];
    if !prefix.bytes().all(is_completion_name_byte) {
        return None;
    }
    let mut end = cursor;
    while end < input.len() && is_completion_name_byte(input.as_bytes()[end]) {
        end += 1;
    }
    Some(SlashCompletionContext {
        start,
        end,
        prefix,
        commands_allowed: start == 0,
    })
}

fn is_completion_name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
}

fn is_builtin_command(prompt: &str) -> bool {
    matches!(
        prompt,
        "/help" | "/model" | "/models" | "/skills" | "/clear" | "/quit"
    )
}

fn builtin_command_from_input(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    (input == trimmed && is_builtin_command(trimmed)).then_some(trimmed)
}

fn cursor_line_column(input: &str, cursor: usize) -> (usize, usize) {
    let before = &input[..cursor];
    let line = before.bytes().filter(|byte| *byte == b'\n').count();
    let column = before
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .chars()
        .count();
    (line, column)
}

fn line_starts(input: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(
        input
            .match_indices('\n')
            .map(|(index, _)| index.saturating_add(1)),
    );
    starts
}

fn byte_index_at_char_column(input: &str, start: usize, end: usize, column: usize) -> usize {
    input[start..end]
        .char_indices()
        .nth(column)
        .map(|(offset, _)| start + offset)
        .unwrap_or(end)
}

fn speed_tier_index(model: &ModelCatalogEntry, selected: Option<&str>) -> usize {
    selected
        .and_then(|selected| {
            model
                .available_service_tiers()
                .iter()
                .position(|tier| tier.id == selected)
                .map(|index| index + 1)
        })
        .unwrap_or(0)
}

fn hierarchical_model_label(model: &ModelCatalogEntry, catalog: &[ModelCatalogEntry]) -> String {
    let Some((family_slug, variant_slug)) = model.slug.rsplit_once('-') else {
        return model.label().to_owned();
    };
    let sibling_prefix = format!("{family_slug}-");
    let siblings = catalog
        .iter()
        .filter(|candidate| {
            candidate
                .slug
                .strip_prefix(&sibling_prefix)
                .is_some_and(|suffix| !suffix.contains('-'))
        })
        .count();
    if siblings < 2 {
        return model.label().to_owned();
    }
    let label = model.label();
    let lower_label = label.to_ascii_lowercase();
    let lower_variant = variant_slug.to_ascii_lowercase();
    let (family, variant) = if lower_label.ends_with(&lower_variant) {
        let split = label.len().saturating_sub(variant_slug.len());
        (
            label[..split]
                .trim_end_matches([' ', '-', '/', ':'])
                .to_owned(),
            label[split..].to_owned(),
        )
    } else {
        (family_slug.to_owned(), title_case(variant_slug))
    };
    format!("{family}  ›  {variant}")
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn compact_path(path: &str, max: usize) -> String {
    if max == 0 || path.chars().count() <= max {
        return path.to_owned();
    }
    let tail = path
        .chars()
        .rev()
        .take(max.saturating_sub(1))
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("…{tail}")
}

pub(crate) fn print_sessions(sessions: &[SessionSummary]) {
    if sessions.is_empty() {
        println!("No saved sessions.");
        return;
    }
    println!("{:<10}  {:<20}  {:<18}  TITLE", "ID", "UPDATED", "MODEL");
    for session in sessions {
        println!(
            "{:<10}  {:<20}  {:<18}  {}",
            &session.id.to_string()[..8],
            session.updated_at.format("%Y-%m-%d %H:%M"),
            session.model,
            session.title
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    use crate::{
        config::Config,
        provider::{ModelCatalogEntry, OpenAiCompatible, ReasoningOption, ServiceTierOption},
        skills::SkillRegistry,
        tools::{ApprovalMode, ToolBox},
    };

    fn test_app(root: &std::path::Path) -> App {
        let config = Config {
            auth: "api_key".into(),
            api_key_env: String::new(),
            ..Config::default()
        };
        let provider = OpenAiCompatible::new(&config).unwrap();
        let store = SessionStore::new(root).unwrap();
        let session = store.create(config.model).unwrap();
        let tools = ToolBox::new(root.to_path_buf(), ApprovalMode::Ask);
        App::new(
            Agent::new(provider, tools, SkillRegistry::default(), session).unwrap(),
            Vec::new(),
            None,
        )
    }

    fn render_text(width: u16, height: u16) -> String {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn composer_edits_unicode_at_character_boundaries() {
        let (line, column) = cursor_line_column("hola\n🦀", "hola\n🦀".len());
        assert_eq!((line, column), (1, 1));
    }

    #[test]
    fn long_paths_keep_the_useful_tail() {
        assert_eq!(compact_path("one/two/three", 8), "…o/three");
    }

    fn add_test_skill(app: &mut App) {
        app.skills.push(SkillView {
            name: "review-rust".into(),
            description: "Review Rust changes.".into(),
            scope: "project",
        });
    }

    #[test]
    fn slash_menu_combines_commands_and_skills_only_at_the_start() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        add_test_skill(&mut app);

        app.insert("/");
        let menu = app.completion.as_ref().unwrap();
        assert!(
            menu.items
                .iter()
                .any(|item| item.kind == CompletionKind::Command && item.name == "help")
        );
        assert!(
            menu.items
                .iter()
                .any(|item| item.kind == CompletionKind::Skill && item.name == "review-rust")
        );

        app.input.clear();
        app.cursor = 0;
        app.completion = None;
        app.insert("Review this /");
        let menu = app.completion.as_ref().unwrap();
        assert!(
            menu.items
                .iter()
                .all(|item| item.kind == CompletionKind::Skill)
        );
    }

    #[test]
    fn accepting_a_skill_completion_inserts_a_slash_mention() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        add_test_skill(&mut app);
        app.insert("Please /rev");

        assert!(app.accept_completion());
        assert_eq!(app.input, "Please /review-rust ");
        assert!(app.completion.is_none());
    }

    #[test]
    fn builtins_are_commands_only_when_they_are_the_entire_input() {
        assert_eq!(builtin_command_from_input("/help"), Some("/help"));
        assert_eq!(builtin_command_from_input("Explain /help"), None);
        assert_eq!(builtin_command_from_input(" /help"), None);
        assert_eq!(builtin_command_from_input("/help "), None);
        assert_eq!(builtin_command_from_input("/review-rust"), None);
    }

    #[test]
    fn printable_characters_use_the_terminal_resolved_keyboard_layout() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path()).unwrap();
        let mut app = test_app(root.path());

        // Spanish layout: AltGr+2 is reported by Windows as Ctrl+Alt+'@'.
        app.handle_key(
            KeyEvent::new(
                KeyCode::Char('@'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            &store,
        )
        .unwrap();
        // US layout: Shift+2 is already reported as the logical '@' character.
        app.handle_key(
            KeyEvent::new(KeyCode::Char('@'), KeyModifiers::SHIFT),
            &store,
        )
        .unwrap();
        app.handle_key(
            KeyEvent::new(
                KeyCode::Char('€'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            &store,
        )
        .unwrap();

        assert_eq!(app.input, "@@€");

        // An unknown Ctrl-only chord remains a control chord rather than text.
        app.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &store,
        )
        .unwrap();
        assert_eq!(app.input, "@@€");
    }

    #[test]
    fn renders_wide_and_compact_layouts() {
        let wide = render_text(120, 36);
        assert!(wide.contains("CODECRAB"));
        assert!(!wide.contains("Activity"));
        assert!(wide.contains("default"));
        assert!(!wide.contains("What are we building?"));
        assert!(!wide.contains("Enter send"));

        let compact = render_text(70, 24);
        assert!(compact.contains("CODECRAB"));
        assert!(!compact.contains("Activity"));
        assert!(compact.contains("Message"));
    }

    #[test]
    fn renders_the_skills_overlay() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.show_skills = true;
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("Skills (0)"));
        assert!(text.contains(".agents/skills"));
    }

    #[test]
    fn renders_and_uses_interactive_skill_picker() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        add_test_skill(&mut app);
        app.open_skill_picker();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("/review-rust"));
        assert!(text.contains("Enter/Tab insert"));

        app.accept_skill_selection();
        assert_eq!(app.input, "/review-rust ");
        assert!(!app.show_skills);
    }

    #[test]
    fn renders_contextual_slash_menu() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        add_test_skill(&mut app);
        app.insert("/");
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("Slash menu"));
        assert!(text.contains("/help"));
        assert!(text.contains("/review-rust"));
    }

    #[test]
    fn vertical_arrows_preserve_the_preferred_text_column() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.input = "abcd\nx\nwxyz".into();
        app.cursor = 4;

        assert!(app.move_vertical(1));
        assert_eq!(app.cursor, 6);
        assert!(app.move_vertical(1));
        assert_eq!(app.cursor, app.input.len());
        assert!(app.move_vertical(-1));
        assert_eq!(app.cursor, 6);
    }

    #[test]
    fn at_menu_completes_files_folders_and_parent_paths() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(workspace.join("src")).unwrap();
        fs::write(workspace.join("hello.txt"), "hello").unwrap();
        fs::write(temp.path().join("above.md"), "above").unwrap();
        let mut app = test_app(&workspace);

        app.insert("@");
        let menu = app.completion.as_ref().unwrap();
        assert_eq!(menu.items[0].kind, CompletionKind::Directory);
        assert!(menu.items.iter().any(|item| {
            item.kind == CompletionKind::Directory
                && item.name == "src"
                && item.icon == Some(NERD_FOLDER)
        }));
        assert!(
            menu.items
                .iter()
                .any(|item| { item.kind == CompletionKind::File && item.name == "hello.txt" })
        );
        let hello = menu
            .items
            .iter()
            .position(|item| item.name == "hello.txt")
            .unwrap();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains(&format!("{NERD_FOLDER} src")));
        assert!(!rendered.contains("folder"));
        assert!(!rendered.contains(" file "));

        app.completion.as_mut().unwrap().selected = hello;
        assert!(app.accept_completion());
        assert_eq!(app.input, "@hello.txt ");

        app.input.clear();
        app.cursor = 0;
        app.insert("@../abo");
        assert!(
            app.completion
                .as_ref()
                .unwrap()
                .items
                .iter()
                .any(|item| item.name == "../above.md")
        );

        let current = file_completion_context("@", 1, &workspace).unwrap();
        assert_eq!(current.directory, workspace.clone());
        let two_levels = file_completion_context("@../../", 7, &workspace).unwrap();
        assert_eq!(
            two_levels.directory,
            workspace.join(format!(
                "..{}..{}",
                std::path::MAIN_SEPARATOR,
                std::path::MAIN_SEPARATOR
            ))
        );
        let context = file_completion_context("@/", 2, &workspace).unwrap();
        assert_eq!(
            fs::canonicalize(context.directory).unwrap(),
            fs::canonicalize(workspace.ancestors().last().unwrap()).unwrap()
        );
        assert_eq!(file_icon(Path::new("app.js")), "");
        assert_eq!(file_icon(Path::new("script.py")), "");
    }

    #[test]
    fn model_picker_walks_model_reasoning_and_service_tier() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path()).unwrap();
        let mut app = test_app(root.path());
        app.model_catalog = vec![
            ModelCatalogEntry {
                slug: "future-9-sol".into(),
                display_name: "Future-9-Sol".into(),
                default_reasoning_level: Some("low".into()),
                supported_reasoning_levels: vec![
                    ReasoningOption {
                        effort: "low".into(),
                        description: "Quick".into(),
                    },
                    ReasoningOption {
                        effort: "deep".into(),
                        description: "Deep".into(),
                    },
                ],
                service_tiers: vec![ServiceTierOption {
                    id: "priority".into(),
                    name: "Fast".into(),
                    description: "Faster".into(),
                }],
                ..ModelCatalogEntry::from_id("future-9-sol".into())
            },
            ModelCatalogEntry::from_id("future-9-terra".into()),
        ];

        app.open_model_picker();
        app.accept_model_selection(&store).unwrap();
        assert_eq!(
            app.model_picker.as_ref().unwrap().step,
            ModelPickerStep::Reasoning
        );
        app.model_picker.as_mut().unwrap().selected = 1;
        app.accept_model_selection(&store).unwrap();
        assert_eq!(
            app.model_picker.as_ref().unwrap().step,
            ModelPickerStep::Speed
        );
        app.model_picker.as_mut().unwrap().selected = 1;
        app.accept_model_selection(&store).unwrap();

        assert_eq!(app.model, "future-9-sol");
        assert_eq!(app.reasoning_effort.as_deref(), Some("deep"));
        assert_eq!(app.service_tier.as_deref(), Some("priority"));
        assert!(app.uses_fast_service_tier());
        assert_eq!(
            app.agent
                .as_ref()
                .unwrap()
                .session()
                .service_tier
                .as_deref(),
            Some("priority")
        );
        assert_eq!(
            hierarchical_model_label(&app.model_catalog[0], &app.model_catalog),
            "Future-9  ›  Sol"
        );

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("future-9-sol"));
        assert!(text.contains("deep"));
        assert!(text.contains('⚡'));
    }
}
