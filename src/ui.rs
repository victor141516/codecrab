use std::{
    collections::{HashMap, HashSet},
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
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
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Style as SyntectStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
};
use tokio::{sync::mpsc, task::JoinHandle};
use unicode_width::UnicodeWidthChar;

use crate::{
    agent::{Agent, turn_was_cancelled},
    audio::AudioRecording,
    completion::{
        CompletionKind, CompletionMenu, CompletionSearch, builtin_command_from_input,
        complete_progressive, goal_objective_from_input,
    },
    config::{
        Config, ConfigStore, ProviderConfig, SessionRegistry, normalized_root, paths_equal,
        validate_provider_name,
    },
    conversation::{
        ConversationHandle, ConversationLifecycle, ConversationManager, ConversationSnapshot,
        ConversationTurn,
    },
    events::{ActivityKind, ActivityStatus, AgentActivity, AgentEvent},
    provider::{Message, ModelCatalogEntry, ModelSelection, OpenAiCompatible, Role},
    session::{
        AgentTurn, ConversationGraphNode, Goal, GoalStatus, Session, SessionProject, SessionStore,
        list_session_projects,
    },
    skills::SkillRegistry,
    tools::ToolBox,
    transcription::Transcriber,
};
use uuid::Uuid;

const CRAB: Color = Color::Rgb(244, 99, 86);
const AQUA: Color = Color::Rgb(74, 210, 200);
const GOAL: Color = Color::Rgb(190, 150, 255);
const MUTED: Color = Color::Rgb(125, 135, 150);
const MARKDOWN_H1: Color = Color::Rgb(255, 179, 71);
const MARKDOWN_HEADING: Color = Color::Rgb(92, 207, 230);
const MARKDOWN_BOLD: Color = Color::Rgb(255, 214, 102);
const MARKDOWN_ITALIC: Color = Color::Rgb(190, 150, 255);
const MARKDOWN_CODE: Color = Color::Rgb(126, 216, 160);
const MARKDOWN_LIST: Color = Color::Rgb(244, 99, 86);
const MARKDOWN_LINK: Color = Color::Rgb(100, 180, 255);
const MARKDOWN_FENCE: Color = Color::Rgb(105, 115, 130);
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const WAVEFORM: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
struct SkillView {
    name: String,
    description: String,
    scope: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModelPickerStep {
    Model,
    Reasoning,
    Speed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditorAction {
    MoveWordLeft,
    MoveWordRight,
    DeleteWordLeft,
    DeleteWordRight,
    MoveLineStart,
    MoveLineEnd,
    DeleteToLineStart,
    DeleteToLineEnd,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ComposerRow {
    start: usize,
    end: usize,
}

struct ModelPicker {
    step: ModelPickerStep,
    selected: usize,
    model_index: usize,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
}

struct SessionPicker {
    projects: Vec<SessionProjectView>,
    selected: usize,
}

struct GoalPicker {
    selected: usize,
    describing: bool,
    description_scroll: u16,
}

enum PendingGoalAction {
    Create(String),
    Toggle(Uuid),
    Pause(Uuid),
    Delete(Uuid),
    BeginEdit(Uuid),
}

#[derive(Default)]
struct GoalButtons {
    toggle: Option<Rect>,
    edit: Option<Rect>,
    delete: Option<Rect>,
    list: Option<Rect>,
}

struct SessionProjectView {
    project: SessionProject,
    expanded: bool,
}

#[derive(Clone, Copy)]
enum SessionPickerRow {
    Project(usize),
    Session(usize, usize),
}

#[derive(Clone)]
struct SourceRange {
    line: usize,
    start: usize,
    end: usize,
}

struct CopyTarget {
    ranges: Vec<SourceRange>,
    text: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TurnKey {
    session_id: Uuid,
    message_index: usize,
}

struct TurnToggleTarget {
    line: usize,
    key: TurnKey,
}

struct ConversationSource {
    lines: Vec<Line<'static>>,
    copy_targets: Vec<CopyTarget>,
    turn_toggles: Vec<TurnToggleTarget>,
    node_lines: Vec<(Uuid, usize)>,
}

#[derive(Clone)]
struct VisualUnit {
    text: String,
    width: u16,
    style: Style,
    source_line: usize,
    source_start: usize,
    source_end: usize,
}

#[derive(Clone, Default)]
struct VisualRow {
    source_line: usize,
    units: Vec<VisualUnit>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TextPoint {
    row: usize,
    unit: usize,
}

struct ConversationView {
    area: Rect,
    rows: Vec<VisualRow>,
    scroll: usize,
    copy_targets: Vec<CopyTarget>,
    turn_toggles: Vec<TurnToggleTarget>,
}

struct BranchNavigator {
    nodes: Vec<ConversationGraphNode>,
    rows: Vec<BranchRow>,
    selected: usize,
    original_path: HashSet<Uuid>,
    preview_path: HashSet<Uuid>,
    original_scroll: u16,
    original_auto_scroll: bool,
    offset: usize,
}

struct BranchRow {
    id: Uuid,
    depth: usize,
    ancestor_continues: Vec<bool>,
    is_last: bool,
}

impl ConversationView {
    fn contains(&self, column: u16, row: u16) -> bool {
        column >= self.area.x
            && column < self.area.right()
            && row >= self.area.y
            && row < self.area.bottom()
    }

    fn point_at(&self, column: u16, row: u16, clamp_vertical: bool) -> Option<TextPoint> {
        if self.rows.is_empty() || column < self.area.x || column >= self.area.right() {
            return None;
        }
        let viewport_row = if clamp_vertical {
            row.saturating_sub(self.area.y)
                .min(self.area.height.saturating_sub(1))
        } else {
            if !self.contains(column, row) {
                return None;
            }
            row - self.area.y
        };
        let row_index = (self.scroll + viewport_row as usize).min(self.rows.len() - 1);
        let units = &self.rows[row_index].units;
        if units.is_empty() {
            return Some(TextPoint {
                row: row_index,
                unit: 0,
            });
        }
        let target_column = column.saturating_sub(self.area.x);
        let mut current = 0;
        for (unit, value) in units.iter().enumerate() {
            if target_column < current + value.width.max(1) {
                return Some(TextPoint {
                    row: row_index,
                    unit,
                });
            }
            current += value.width;
        }
        Some(TextPoint {
            row: row_index,
            unit: units.len() - 1,
        })
    }

    fn copy_target_at(&self, point: TextPoint) -> Option<&CopyTarget> {
        let unit = self.rows.get(point.row)?.units.get(point.unit)?;
        self.copy_targets.iter().find(|target| {
            target.ranges.iter().any(|range| {
                range.line == unit.source_line
                    && unit.source_start < range.end
                    && unit.source_end > range.start
            })
        })
    }

    fn turn_toggle_at(&self, point: TextPoint) -> Option<TurnKey> {
        let source_line = self.rows.get(point.row)?.source_line;
        self.turn_toggles
            .iter()
            .find(|target| target.line == source_line)
            .map(|target| target.key)
    }

    fn selected_text(&self, selection: &TextSelection) -> String {
        let (start, end) = selection.normalized();
        let mut output = String::new();
        for row_index in start.row..=end.row {
            if row_index > start.row
                && self.rows.get(row_index - 1).map(|row| row.source_line)
                    != self.rows.get(row_index).map(|row| row.source_line)
            {
                output.push('\n');
            }
            let Some(row) = self.rows.get(row_index) else {
                continue;
            };
            if row.units.is_empty() {
                continue;
            }
            let first = if row_index == start.row {
                start.unit.min(row.units.len() - 1)
            } else {
                0
            };
            let last = if row_index == end.row {
                end.unit.min(row.units.len() - 1)
            } else {
                row.units.len() - 1
            };
            for unit in &row.units[first..=last] {
                output.push_str(&unit.text);
            }
        }
        output
    }
}

struct TextSelection {
    anchor: TextPoint,
    cursor: TextPoint,
    dragging: bool,
    moved: bool,
    last_column: u16,
    last_row: u16,
}

impl TextSelection {
    fn normalized(&self) -> (TextPoint, TextPoint) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    fn contains(&self, point: TextPoint) -> bool {
        let (start, end) = self.normalized();
        point >= start && point <= end
    }
}

struct CopyFlash {
    ranges: Vec<SourceRange>,
    until: Instant,
}

struct TurnAnchor {
    key: TurnKey,
    viewport_row: usize,
}

struct MarkdownHighlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl MarkdownHighlighter {
    fn new() -> Self {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .get("base16-ocean.dark")
            .or_else(|| themes.themes.values().next())
            .expect("syntect ships at least one default theme")
            .clone();
        Self { syntaxes, theme }
    }

    fn render(&self, markdown: &str) -> Vec<Line<'static>> {
        let lines = markdown.lines().collect::<Vec<_>>();
        let mut rendered = Vec::with_capacity(lines.len());
        let mut index = 0;
        while index < lines.len() {
            let line = lines[index];
            let Some(language) = markdown_fence_language(line) else {
                rendered.push(markdown_inline_line(line));
                index += 1;
                continue;
            };

            rendered.push(markdown_fence_line(line));
            index += 1;
            let syntax = self
                .syntaxes
                .find_syntax_by_token(language)
                .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
            let mut highlighter = HighlightLines::new(syntax, &self.theme);
            while index < lines.len() && !markdown_fence_closes(lines[index]) {
                rendered.push(self.highlight_code_line(&mut highlighter, lines[index]));
                index += 1;
            }
            if index < lines.len() {
                rendered.push(markdown_fence_line(lines[index]));
                index += 1;
            }
        }
        rendered
    }

    fn highlight_code_line(
        &self,
        highlighter: &mut HighlightLines<'_>,
        line: &str,
    ) -> Line<'static> {
        let with_newline = format!("{line}\n");
        let Ok(regions) = highlighter.highlight_line(&with_newline, &self.syntaxes) else {
            return Line::from(Span::styled(line.to_owned(), markdown_code_style()));
        };
        let mut remaining = line.len();
        let mut spans = Vec::new();
        for (style, text) in regions {
            if remaining == 0 {
                break;
            }
            let take = text.len().min(remaining);
            if take > 0 {
                spans.push(Span::styled(
                    text[..take].to_owned(),
                    ratatui_style_from_syntect(style),
                ));
                remaining -= take;
            }
        }
        Line::from(spans)
    }
}

fn shared_markdown_highlighter() -> Arc<MarkdownHighlighter> {
    static HIGHLIGHTER: OnceLock<Arc<MarkdownHighlighter>> = OnceLock::new();
    HIGHLIGHTER
        .get_or_init(|| Arc::new(MarkdownHighlighter::new()))
        .clone()
}

fn markdown_fence_language(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    trimmed.strip_prefix("```").map(|language| language.trim())
}

fn markdown_fence_closes(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

fn markdown_fence_line(line: &str) -> Line<'static> {
    let Some(marker_start) = line.find("```") else {
        return Line::from(line.to_owned());
    };
    let marker_end = marker_start + 3;
    let mut spans = Vec::new();
    if marker_start > 0 {
        spans.push(Span::raw(line[..marker_start].to_owned()));
    }
    spans.push(Span::styled(
        line[marker_start..marker_end].to_owned(),
        Style::default().fg(MARKDOWN_FENCE),
    ));
    if marker_end < line.len() {
        spans.push(Span::styled(
            line[marker_end..].to_owned(),
            Style::default()
                .fg(MARKDOWN_CODE)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn markdown_inline_line(line: &str) -> Line<'static> {
    if let Some(level) = markdown_heading_level(line) {
        let color = if level == 1 {
            MARKDOWN_H1
        } else {
            MARKDOWN_HEADING
        };
        return Line::from(Span::styled(
            line.to_owned(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }

    let marker = markdown_list_marker(line).or_else(|| markdown_quote_marker(line));
    let Some((start, end)) = marker else {
        return Line::from(markdown_inline_spans(line));
    };
    let mut spans = Vec::new();
    if start > 0 {
        spans.push(Span::raw(line[..start].to_owned()));
    }
    spans.push(Span::styled(
        line[start..end].to_owned(),
        Style::default()
            .fg(MARKDOWN_LIST)
            .add_modifier(Modifier::BOLD),
    ));
    spans.extend(markdown_inline_spans(&line[end..]));
    Line::from(spans)
}

fn markdown_heading_level(line: &str) -> Option<usize> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let level = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    (level > 0
        && level <= 6
        && trimmed
            .as_bytes()
            .get(level)
            .is_some_and(u8::is_ascii_whitespace))
    .then_some(level)
}

fn markdown_list_marker(line: &str) -> Option<(usize, usize)> {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    let rest = &line[indent..];
    if rest.starts_with("- ") || rest.starts_with("+ ") || rest.starts_with("* ") {
        return Some((indent, indent + 1));
    }
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0
        && rest
            .as_bytes()
            .get(digits)
            .is_some_and(|byte| matches!(byte, b'.' | b')'))
        && rest
            .as_bytes()
            .get(digits + 1)
            .is_some_and(u8::is_ascii_whitespace)
    {
        return Some((indent, indent + digits + 1));
    }
    None
}

fn markdown_quote_marker(line: &str) -> Option<(usize, usize)> {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    line.as_bytes()
        .get(indent)
        .is_some_and(|byte| *byte == b'>')
        .then_some((indent, indent + 1))
}

fn markdown_inline_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut plain_start = 0;
    let mut index = 0;
    while index < text.len() {
        let match_result = markdown_inline_match(text, index);
        let Some((end, style)) = match_result else {
            index += text[index..].chars().next().unwrap().len_utf8();
            continue;
        };
        if plain_start < index {
            spans.push(Span::raw(text[plain_start..index].to_owned()));
        }
        spans.push(Span::styled(text[index..end].to_owned(), style));
        index = end;
        plain_start = end;
    }
    if plain_start < text.len() {
        spans.push(Span::raw(text[plain_start..].to_owned()));
    }
    spans
}

fn markdown_inline_match(text: &str, index: usize) -> Option<(usize, Style)> {
    if index > 0 && text.as_bytes().get(index - 1) == Some(&b'\\') {
        return None;
    }
    let rest = &text[index..];
    if rest.starts_with('`') && !rest.starts_with("```") {
        return markdown_delimited_end(rest, "`")
            .map(|end| (index + end, Style::default().fg(MARKDOWN_CODE)));
    }
    for marker in ["**", "__"] {
        if rest.starts_with(marker) {
            return markdown_delimited_end(rest, marker).map(|end| {
                (
                    index + end,
                    Style::default()
                        .fg(MARKDOWN_BOLD)
                        .add_modifier(Modifier::BOLD),
                )
            });
        }
    }
    for marker in ["*", "_"] {
        if rest.starts_with(marker) && !rest.starts_with(&marker.repeat(2)) {
            return markdown_delimited_end(rest, marker).map(|end| {
                (
                    index + end,
                    Style::default()
                        .fg(MARKDOWN_ITALIC)
                        .add_modifier(Modifier::ITALIC),
                )
            });
        }
    }
    if rest.starts_with('[')
        && let Some(label_end) = rest.find("](")
        && let Some(url_end) = rest[label_end + 2..].find(')')
    {
        return Some((
            index + label_end + 2 + url_end + 1,
            Style::default()
                .fg(MARKDOWN_LINK)
                .add_modifier(Modifier::UNDERLINED),
        ));
    }
    None
}

fn markdown_delimited_end(text: &str, marker: &str) -> Option<usize> {
    let content_start = marker.len();
    let closing = text[content_start..].find(marker)? + content_start;
    (closing > content_start).then_some(closing + marker.len())
}

fn markdown_code_style() -> Style {
    Style::default().fg(MARKDOWN_CODE)
}

fn ratatui_style_from_syntect(style: SyntectStyle) -> Style {
    let mut output = Style::default().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        output = output.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        output = output.add_modifier(Modifier::UNDERLINED);
    }
    output
}

impl SessionPicker {
    fn rows(&self) -> Vec<SessionPickerRow> {
        let mut rows = Vec::new();
        for (project_index, project) in self.projects.iter().enumerate() {
            rows.push(SessionPickerRow::Project(project_index));
            if project.expanded {
                rows.extend(
                    (0..project.project.sessions.len()).map(|session_index| {
                        SessionPickerRow::Session(project_index, session_index)
                    }),
                );
            }
        }
        rows
    }

    fn selected_row(&self) -> Option<SessionPickerRow> {
        self.rows().get(self.selected).copied()
    }

    fn select_project(&mut self, project_index: usize) {
        if let Some(index) = self.rows().iter().position(
            |row| matches!(row, SessionPickerRow::Project(index) if *index == project_index),
        ) {
            self.selected = index;
        }
    }
}

struct App {
    conversations: ConversationManager,
    conversation: ConversationHandle,
    transcript: Vec<Message>,
    transcript_node_ids: Vec<Uuid>,
    live_messages: Vec<Message>,
    activities: Vec<AgentActivity>,
    turns: Vec<AgentTurn>,
    running: Option<JoinHandle<Result<ConversationTurn>>>,
    event_rx: Option<mpsc::UnboundedReceiver<AgentEvent>>,
    recording: Option<AudioRecording>,
    transcription: Option<JoinHandle<Result<String>>>,
    send_after_transcription: bool,
    debug_openai: bool,
    config: Config,
    registry: SessionRegistry,
    clipboard: Option<arboard::Clipboard>,
    markdown: Arc<MarkdownHighlighter>,
    input: String,
    cursor: usize,
    preferred_column: Option<usize>,
    composer_width: usize,
    pending_user: Option<String>,
    queued_prompt: Option<String>,
    goals: Vec<Goal>,
    visible_goal_id: Option<Uuid>,
    goal_picker: Option<GoalPicker>,
    pending_goal_action: Option<PendingGoalAction>,
    editing_goal_id: Option<Uuid>,
    editing_goal_resume: bool,
    goal_buttons: GoalButtons,
    last_escape: Option<Instant>,
    steer_button: Option<Rect>,
    conversation_view: Option<ConversationView>,
    text_selection: Option<TextSelection>,
    copy_flash: Option<CopyFlash>,
    error: Option<String>,
    scroll: u16,
    max_scroll: u16,
    auto_scroll: bool,
    spinner: usize,
    show_help: bool,
    show_skills: bool,
    skill_selection: usize,
    completion: Option<CompletionMenu>,
    completion_search: Option<CompletionSearch>,
    completion_request_id: u64,
    model_catalog: Vec<ModelCatalogEntry>,
    model_picker: Option<ModelPicker>,
    session_picker: Option<SessionPicker>,
    branch_navigator: Option<BranchNavigator>,
    should_quit: bool,
    project: String,
    project_root: PathBuf,
    model: String,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    skills: Vec<SkillView>,
    background_turns: HashMap<Uuid, BackgroundTurn>,
    model_catalogs: HashMap<Uuid, Vec<ModelCatalogEntry>>,
    session_id: Uuid,
    expanded_turns: HashSet<TurnKey>,
    pending_turn_anchor: Option<TurnAnchor>,
    pending_branch_node: Option<Uuid>,
}

struct BackgroundTurn {
    running: JoinHandle<Result<ConversationTurn>>,
    event_rx: mpsc::UnboundedReceiver<AgentEvent>,
    pending_user: Option<String>,
    queued_prompt: Option<String>,
    pending_goal_action: Option<PendingGoalAction>,
    live_messages: Vec<Message>,
}

impl App {
    fn new(
        agent: Agent,
        model_catalog: Vec<ModelCatalogEntry>,
        catalog_error: Option<String>,
        debug_openai: bool,
        config: Config,
        registry: SessionRegistry,
    ) -> Result<Self> {
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
        let transcript = agent.session().messages.to_vec();
        let transcript_node_ids = agent.session().messages.active_node_ids().to_vec();
        let activities = agent.session().activities.clone();
        let turns = agent.session().turns.clone();
        let goals = agent.session().goals.clone();
        let visible_goal_id = agent.session().visible_goal_id;
        let initial_error = catalog_error
            .as_ref()
            .map(|error| format!("Could not load the model catalog: {error}"));
        let conversation = ConversationHandle::spawn(agent, registry.clone())?;
        let session_id = conversation.snapshot().session.id;
        let conversations =
            ConversationManager::with_handle(registry.clone(), conversation.clone());
        let model_catalogs = HashMap::from([(session_id, model_catalog.clone())]);
        Ok(Self {
            conversations,
            conversation,
            transcript,
            transcript_node_ids,
            live_messages: Vec::new(),
            activities,
            turns,
            running: None,
            event_rx: None,
            recording: None,
            transcription: None,
            send_after_transcription: false,
            debug_openai,
            config,
            registry,
            clipboard: arboard::Clipboard::new().ok(),
            markdown: shared_markdown_highlighter(),
            input: String::new(),
            cursor: 0,
            preferred_column: None,
            composer_width: 80,
            pending_user: None,
            queued_prompt: None,
            goals,
            visible_goal_id,
            goal_picker: None,
            pending_goal_action: None,
            editing_goal_id: None,
            editing_goal_resume: false,
            goal_buttons: GoalButtons::default(),
            last_escape: None,
            steer_button: None,
            conversation_view: None,
            text_selection: None,
            copy_flash: None,
            error: initial_error,
            scroll: 0,
            max_scroll: 0,
            auto_scroll: true,
            spinner: 0,
            show_help: false,
            show_skills: false,
            skill_selection: 0,
            completion: None,
            completion_search: None,
            completion_request_id: 0,
            model_catalog,
            model_picker: None,
            session_picker: None,
            branch_navigator: None,
            should_quit: false,
            project,
            project_root,
            model,
            reasoning_effort,
            service_tier,
            skills,
            background_turns: HashMap::new(),
            model_catalogs,
            session_id,
            expanded_turns: HashSet::new(),
            pending_turn_anchor: None,
            pending_branch_node: None,
        })
    }

    fn is_running(&self) -> bool {
        self.running.is_some()
    }

    fn is_busy(&self) -> bool {
        self.is_running() || self.recording.is_some() || self.transcription.is_some()
    }

    fn status(&self) -> String {
        if self.recording.is_some() {
            "● recording".into()
        } else if self.transcription.is_some() {
            format!("{} transcribing", SPINNER[self.spinner % SPINNER.len()])
        } else if self.is_running() {
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

    fn handle_paste(&mut self, text: &str) -> bool {
        if self.recording.is_some()
            || self.transcription.is_some()
            || self.goal_picker.is_some()
            || self.session_picker.is_some()
            || self.model_picker.is_some()
            || self.show_help
            || self.show_skills
        {
            return false;
        }
        let normalized = normalize_paste(text);
        if normalized.is_empty() {
            return false;
        }
        self.insert(&normalized);
        true
    }

    fn insert_char(&mut self, value: char) {
        self.input.insert(self.cursor, value);
        self.cursor += value.len_utf8();
        self.preferred_column = None;
        self.refresh_completion();
    }

    fn insert_transcript(&mut self, transcript: &str) -> bool {
        let transcript = transcript.trim();
        if transcript.is_empty() {
            return false;
        }
        let leading_space = self.cursor > 0
            && !self.input[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let trailing_space = self.cursor < self.input.len()
            && !self.input[self.cursor..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace);
        let mut insertion = String::new();
        if leading_space {
            insertion.push(' ');
        }
        insertion.push_str(transcript);
        if trailing_space {
            insertion.push(' ');
        }
        self.insert(&insertion);
        self.close_completion();
        true
    }

    fn session_provider(&self) -> String {
        self.conversation.snapshot().session.provider
    }

    fn dictation_available(&self) -> bool {
        Transcriber::is_available(&self.config, &self.session_provider()).unwrap_or(false)
    }

    fn toggle_dictation(&mut self) -> Result<()> {
        if self.transcription.is_some() {
            return Ok(());
        }
        if !self.dictation_available() {
            anyhow::bail!(
                "voice dictation requires the official OpenAI provider and valid authentication"
            );
        }
        if self.recording.is_some() {
            self.stop_dictation(false)?;
        } else {
            self.error = None;
            self.close_completion();
            self.send_after_transcription = false;
            self.recording = Some(AudioRecording::start()?);
        }
        Ok(())
    }

    fn stop_dictation(&mut self, send_after_transcription: bool) -> Result<()> {
        let Some(recording) = self.recording.take() else {
            return Ok(());
        };
        self.send_after_transcription = false;
        let audio = recording.finish()?;
        self.send_after_transcription = send_after_transcription;
        let debug_openai = self.debug_openai;
        let config = self.config.clone();
        let provider = self.session_provider();
        self.transcription = Some(tokio::spawn(async move {
            Transcriber::new(&config, &provider, debug_openai)?
                .transcribe(audio, "audio/wav")
                .await
        }));
        Ok(())
    }

    async fn finish_transcription_if_ready(&mut self) -> Result<()> {
        if !self
            .transcription
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            return Ok(());
        }
        let task = self.transcription.take().expect("checked above");
        let send_after_transcription = std::mem::take(&mut self.send_after_transcription);
        match task.await.context("dictation task failed")? {
            Ok(transcript) => {
                let inserted = self.insert_transcript(&transcript);
                self.error = None;
                if send_after_transcription && inserted {
                    self.submit().await?;
                }
            }
            Err(error) => self.error = Some(format!("Dictation failed: {error:#}")),
        }
        Ok(())
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

    fn apply_editor_action(&mut self, action: EditorAction) {
        match action {
            EditorAction::MoveWordLeft => self.cursor = word_left_index(&self.input, self.cursor),
            EditorAction::MoveWordRight => self.cursor = word_right_index(&self.input, self.cursor),
            EditorAction::DeleteWordLeft => {
                let start = word_left_index(&self.input, self.cursor);
                self.input.drain(start..self.cursor);
                self.cursor = start;
            }
            EditorAction::DeleteWordRight => {
                let end = word_right_index(&self.input, self.cursor);
                self.input.drain(self.cursor..end);
            }
            EditorAction::MoveLineStart => {
                self.cursor = hard_line_start(&self.input, self.cursor);
            }
            EditorAction::MoveLineEnd => {
                self.cursor = hard_line_end(&self.input, self.cursor);
            }
            EditorAction::DeleteToLineStart => {
                let start = hard_line_start(&self.input, self.cursor);
                self.input.drain(start..self.cursor);
                self.cursor = start;
            }
            EditorAction::DeleteToLineEnd => {
                let end = hard_line_end(&self.input, self.cursor);
                self.input.drain(self.cursor..end);
            }
        }
        self.preferred_column = None;
        self.refresh_completion();
    }

    fn move_vertical(&mut self, delta: isize) -> bool {
        let rows = composer_rows(&self.input, self.composer_width);
        let current = composer_cursor_position(&self.input, &rows, self.cursor).0;
        let target = current as isize + delta;
        if target < 0 || target >= rows.len() as isize {
            return false;
        }
        let column = self
            .preferred_column
            .unwrap_or_else(|| composer_cursor_position(&self.input, &rows, self.cursor).1);
        self.preferred_column = Some(column);
        self.cursor = byte_index_at_display_column(&self.input, rows[target as usize], column);
        self.refresh_completion();
        true
    }

    fn refresh_completion(&mut self) {
        let previous = self
            .completion
            .as_ref()
            .and_then(|menu| menu.items.get(menu.selected).map(|item| item.id.clone()));
        self.completion_search = None;
        self.completion_request_id = self.completion_request_id.wrapping_add(1);
        let (menu, search) = complete_progressive(
            &self.input,
            self.cursor,
            &self.project_root,
            self.skills
                .iter()
                .map(|skill| (skill.name.as_str(), skill.description.as_str())),
            self.completion_request_id,
        );
        self.completion = menu.map(|mut menu| {
            menu.selected = previous
                .and_then(|id| menu.items.iter().position(|item| item.id == id))
                .unwrap_or(0);
            menu
        });
        self.completion_search = search;
    }

    fn drain_completion_updates(&mut self) {
        let Some(search) = &mut self.completion_search else {
            return;
        };
        let request_id = search.request_id;
        let token_start = search.token_start;
        let token_end = search.token_end;
        let mut updates = Vec::new();
        let mut finished = false;
        loop {
            match search.try_recv() {
                Ok(update) => updates.push(update),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        for update in updates {
            if request_id != self.completion_request_id
                || update.request_id != self.completion_request_id
                || update.items.is_empty()
            {
                continue;
            }
            let previous = self
                .completion
                .as_ref()
                .and_then(|menu| menu.items.get(menu.selected))
                .map(|item| item.id.clone());
            let selected = previous
                .and_then(|id| update.items.iter().position(|item| item.id == id))
                .unwrap_or(0);
            self.completion = Some(CompletionMenu {
                items: update.items,
                selected,
                token_start,
                token_end,
            });
        }
        if finished {
            self.completion_search = None;
        }
    }

    fn close_completion(&mut self) {
        self.completion = None;
        self.completion_search = None;
        self.completion_request_id = self.completion_request_id.wrapping_add(1);
    }

    fn move_completion(&mut self, delta: isize) {
        let Some(menu) = &mut self.completion else {
            return;
        };
        if menu.items.is_empty() {
            return;
        }
        let len = menu.items.len() as isize;
        menu.selected = (menu.selected as isize + delta).rem_euclid(len) as usize;
    }

    fn accept_completion(&mut self) -> bool {
        let Some(menu) = self.completion.take() else {
            return false;
        };
        self.completion_search = None;
        self.completion_request_id = self.completion_request_id.wrapping_add(1);
        let Some(item) = menu.items.get(menu.selected) else {
            return false;
        };
        self.input
            .replace_range(menu.token_start..menu.token_end, &item.replacement);
        self.cursor = menu.token_start + item.replacement.len();
        if item.kind == CompletionKind::Directory {
            self.refresh_completion();
            return true;
        }
        self.preferred_column = None;
        true
    }

    fn open_skill_picker(&mut self) {
        self.show_skills = true;
        self.skill_selection = self
            .skill_selection
            .min(self.skills.len().saturating_sub(1));
        self.close_completion();
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
        self.close_completion();
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
        self.close_completion();
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

    async fn accept_model_selection(&mut self) -> Result<()> {
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
                let snapshot = self.conversation.set_model(selection).await?;
                self.apply_snapshot(snapshot);
                self.error = None;
                self.model_picker = None;
            }
        }
        Ok(())
    }

    async fn save_active_session(&self) -> Result<()> {
        self.conversation.persist_if_idle().await?;
        Ok(())
    }

    fn apply_snapshot(&mut self, snapshot: ConversationSnapshot) {
        self.transcript = snapshot.session.messages.to_vec();
        self.transcript_node_ids = snapshot.session.messages.active_node_ids().to_vec();
        self.activities.clone_from(&snapshot.session.activities);
        self.turns.clone_from(&snapshot.session.turns);
        self.goals.clone_from(&snapshot.session.goals);
        self.visible_goal_id = snapshot.session.visible_goal_id;
        self.session_id = snapshot.session.id;
        self.project_root = snapshot.project_root;
        self.project = self.project_root.display().to_string();
        self.model.clone_from(&snapshot.session.model);
        self.reasoning_effort
            .clone_from(&snapshot.session.reasoning_effort);
        self.service_tier.clone_from(&snapshot.session.service_tier);
        self.skills = snapshot
            .skills
            .into_iter()
            .map(|skill| SkillView {
                name: skill.name,
                description: skill.description,
                scope: skill.scope,
            })
            .collect();
    }

    fn apply_branch_session(&mut self, session: &Session) {
        self.transcript = session.messages.to_vec();
        self.transcript_node_ids = session.messages.active_node_ids().to_vec();
        self.activities.clone_from(&session.activities);
        self.turns.clone_from(&session.turns);
    }

    fn open_branch_navigator(&mut self) {
        if self.is_busy() {
            self.error = Some("Wait for the active operation before browsing branches.".into());
            return;
        }
        let snapshot = self.conversation.snapshot();
        let nodes = snapshot.session.messages.visible_user_nodes();
        if nodes.is_empty() {
            self.error = Some("This conversation has no visible user messages.".into());
            return;
        }
        let active_path = snapshot
            .session
            .messages
            .active_node_ids()
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let selected = nodes
            .iter()
            .rposition(|node| active_path.contains(&node.id))
            .unwrap_or(0);
        let rows = branch_rows(&nodes);
        self.branch_navigator = Some(BranchNavigator {
            nodes,
            rows,
            selected,
            original_path: active_path.clone(),
            preview_path: active_path,
            original_scroll: self.scroll,
            original_auto_scroll: self.auto_scroll,
            offset: 0,
        });
        self.show_help = false;
        self.show_skills = false;
        self.model_picker = None;
        self.session_picker = None;
        self.goal_picker = None;
        self.close_completion();
        self.error = None;
    }

    fn move_branch_selection(&mut self, direction: KeyCode) -> Result<()> {
        let Some(navigator) = self.branch_navigator.as_ref() else {
            return Ok(());
        };
        let selected_id = navigator.nodes[navigator.selected].id;
        let target = match direction {
            KeyCode::Up => navigator.selected.checked_sub(1),
            KeyCode::Down => {
                (navigator.selected + 1 < navigator.nodes.len()).then_some(navigator.selected + 1)
            }
            KeyCode::Left => navigator.nodes[navigator.selected]
                .parent_id
                .and_then(|parent_id| navigator.nodes.iter().position(|node| node.id == parent_id)),
            KeyCode::Right => navigator
                .nodes
                .iter()
                .position(|node| node.parent_id == Some(selected_id)),
            _ => None,
        };
        let Some(target) = target else {
            return Ok(());
        };
        if let Some(navigator) = &mut self.branch_navigator {
            navigator.selected = target;
        }
        self.preview_selected_branch()
    }

    fn preview_selected_branch(&mut self) -> Result<()> {
        let Some(navigator) = self.branch_navigator.as_ref() else {
            return Ok(());
        };
        let selected_id = navigator.nodes[navigator.selected].id;
        let preview = self
            .conversation
            .snapshot()
            .session
            .preview_branch(selected_id)?;
        let preview_path = preview.messages.active_node_ids().iter().copied().collect();
        self.apply_branch_session(&preview);
        if let Some(navigator) = &mut self.branch_navigator {
            navigator.preview_path = preview_path;
        }
        self.pending_branch_node = Some(selected_id);
        self.auto_scroll = false;
        Ok(())
    }

    fn cancel_branch_navigator(&mut self) {
        let snapshot = self.conversation.snapshot();
        self.apply_branch_session(&snapshot.session);
        if let Some(navigator) = self.branch_navigator.take() {
            self.scroll = navigator.original_scroll;
            self.auto_scroll = navigator.original_auto_scroll;
        }
        self.pending_branch_node = None;
    }

    async fn confirm_branch_selection(&mut self) -> Result<()> {
        let Some(navigator) = self.branch_navigator.as_ref() else {
            return Ok(());
        };
        let node_id = navigator.nodes[navigator.selected].id;
        let snapshot = self.conversation.select_branch(node_id).await?;
        self.apply_snapshot(snapshot);
        self.branch_navigator = None;
        self.pending_branch_node = Some(node_id);
        self.auto_scroll = false;
        self.error = None;
        Ok(())
    }

    fn visible_goal(&self) -> Option<&Goal> {
        self.visible_goal_id
            .and_then(|id| self.goals.iter().find(|goal| goal.id == id))
    }

    fn active_goal_id(&self) -> Option<Uuid> {
        self.goals
            .iter()
            .find(|goal| goal.status == GoalStatus::Active)
            .map(|goal| goal.id)
    }

    fn request_goal_action(&mut self, action: PendingGoalAction) {
        self.pending_goal_action = Some(action);
        if self.is_running() {
            self.cancel_current_turn();
        }
    }

    fn request_stop(&mut self) {
        if let Some(id) = self.active_goal_id() {
            self.pending_goal_action = Some(PendingGoalAction::Pause(id));
        }
        self.cancel_current_turn();
    }

    fn open_goal_picker(&mut self) {
        let selected = self
            .visible_goal_id
            .and_then(|id| self.goals.iter().position(|goal| goal.id == id))
            .unwrap_or(0);
        self.goal_picker = Some(GoalPicker {
            selected,
            describing: false,
            description_scroll: 0,
        });
        self.show_help = false;
        self.show_skills = false;
        self.model_picker = None;
        self.session_picker = None;
        self.close_completion();
    }

    fn move_goal_selection(&mut self, delta: isize) {
        let Some(picker) = &mut self.goal_picker else {
            return;
        };
        if picker.describing {
            picker.description_scroll = if delta < 0 {
                picker
                    .description_scroll
                    .saturating_sub(delta.unsigned_abs() as u16)
            } else {
                picker.description_scroll.saturating_add(delta as u16)
            };
            return;
        }
        if self.goals.is_empty() {
            return;
        }
        picker.selected =
            (picker.selected as isize + delta).rem_euclid(self.goals.len() as isize) as usize;
    }

    fn describe_selected_goal(&mut self) {
        let Some(picker) = &mut self.goal_picker else {
            return;
        };
        if self.goals.get(picker.selected).is_some() {
            picker.describing = !picker.describing;
            picker.description_scroll = 0;
        }
    }

    fn toggle_selected_goal(&mut self) {
        let Some(id) = self
            .goal_picker
            .as_ref()
            .and_then(|picker| self.goals.get(picker.selected))
            .map(|goal| goal.id)
        else {
            return;
        };
        self.goal_picker = None;
        self.request_goal_action(PendingGoalAction::Toggle(id));
    }

    fn delete_selected_goal(&mut self) {
        let Some((index, id)) = self.goal_picker.as_ref().and_then(|picker| {
            self.goals
                .get(picker.selected)
                .map(|goal| (picker.selected, goal.id))
        }) else {
            return;
        };
        self.request_goal_action(PendingGoalAction::Delete(id));
        if let Some(picker) = &mut self.goal_picker {
            picker.selected = index.min(self.goals.len().saturating_sub(2));
            picker.describing = false;
        }
    }

    fn begin_goal_edit(&mut self, id: Uuid) {
        let Some(goal) = self.goals.iter().find(|goal| goal.id == id) else {
            return;
        };
        self.input.clone_from(&goal.objective);
        self.cursor = self.input.len();
        self.preferred_column = None;
        self.close_completion();
        self.editing_goal_id = Some(id);
    }

    async fn apply_pending_goal_action(&mut self) -> Result<bool> {
        let Some(action) = self.pending_goal_action.take() else {
            return Ok(false);
        };
        let mut start_prompt = None;
        let mut continue_goal = false;
        let snapshot = match action {
            PendingGoalAction::Create(objective) => {
                start_prompt = Some(objective.clone());
                Some(self.conversation.create_goal(objective).await?)
            }
            PendingGoalAction::Toggle(id) => {
                let active = self
                    .goals
                    .iter()
                    .find(|goal| goal.id == id)
                    .is_some_and(|goal| goal.status == GoalStatus::Active);
                if active {
                    self.conversation.pause_goal(id).await?
                } else {
                    let snapshot = self.conversation.activate_goal(id).await?;
                    continue_goal = snapshot.is_some();
                    snapshot
                }
            }
            PendingGoalAction::Pause(id) => self.conversation.pause_goal(id).await?,
            PendingGoalAction::Delete(id) => self.conversation.delete_goal(id).await?,
            PendingGoalAction::BeginEdit(id) => {
                self.editing_goal_resume = self
                    .goals
                    .iter()
                    .find(|goal| goal.id == id)
                    .is_some_and(|goal| goal.status == GoalStatus::Active);
                let snapshot = if self.editing_goal_resume {
                    self.conversation.pause_goal(id).await?
                } else {
                    Some(self.conversation.snapshot())
                };
                if let Some(snapshot) = snapshot {
                    self.apply_snapshot(snapshot);
                }
                self.begin_goal_edit(id);
                return Ok(false);
            }
        };
        if let Some(snapshot) = snapshot {
            self.apply_snapshot(snapshot);
        }
        if let Some(prompt) = start_prompt {
            self.start_turn(prompt, false)?;
            return Ok(true);
        }
        if continue_goal {
            self.start_goal_continuation()?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn open_session_picker(&mut self) -> Result<()> {
        self.save_active_session().await?;
        let current_id = self.conversation.snapshot().session.id;
        let projects = list_session_projects(&self.project_root, &self.registry)?
            .into_iter()
            .enumerate()
            .map(|(index, project)| SessionProjectView {
                project,
                expanded: index == 0,
            })
            .collect::<Vec<_>>();
        let mut picker = SessionPicker {
            projects,
            selected: 0,
        };
        if let Some(selected) = picker.rows().iter().position(|row| {
            matches!(
                row,
                SessionPickerRow::Session(project, session)
                    if picker.projects[*project].project.sessions[*session].id == current_id
            )
        }) {
            picker.selected = selected;
        }
        self.session_picker = Some(picker);
        self.show_help = false;
        self.show_skills = false;
        self.model_picker = None;
        self.close_completion();
        Ok(())
    }

    fn move_session_selection(&mut self, delta: isize) {
        let Some(picker) = &mut self.session_picker else {
            return;
        };
        let len = picker.rows().len();
        if len == 0 {
            return;
        }
        picker.selected = (picker.selected as isize + delta).rem_euclid(len as isize) as usize;
    }

    fn move_session_left(&mut self) {
        let Some(picker) = &mut self.session_picker else {
            return;
        };
        match picker.selected_row() {
            Some(SessionPickerRow::Session(project, _)) => picker.select_project(project),
            Some(SessionPickerRow::Project(project)) if picker.projects[project].expanded => {
                picker.projects[project].expanded = false;
                picker.select_project(project);
            }
            _ => {}
        }
    }

    fn move_session_right(&mut self) {
        let Some(picker) = &mut self.session_picker else {
            return;
        };
        if let Some(SessionPickerRow::Project(project)) = picker.selected_row()
            && !picker.projects[project].expanded
        {
            picker.projects[project].expanded = true;
            picker.select_project(project);
        }
    }

    async fn accept_session_selection(&mut self) -> Result<()> {
        let Some(row) = self
            .session_picker
            .as_ref()
            .and_then(SessionPicker::selected_row)
        else {
            return Ok(());
        };
        let SessionPickerRow::Session(project_index, session_index) = row else {
            self.move_session_right();
            return Ok(());
        };
        let (root, id) = {
            let picker = self.session_picker.as_ref().unwrap();
            let project = &picker.projects[project_index].project;
            (project.root.clone(), project.sessions[session_index].id)
        };
        self.park_current_turn();
        let mut catalog_error = None;
        self.model_catalogs.insert(
            self.conversation.snapshot().session.id,
            self.model_catalog.clone(),
        );
        self.conversation = if let Some(existing) = self.conversations.get(id) {
            existing
        } else {
            let session = SessionStore::new(&root)?.load(Some(&id.to_string()))?;
            let mut provider = OpenAiCompatible::new(&self.config, &session.provider)?;
            provider.set_debug_openai(self.debug_openai);
            let mut agent = Agent::new(
                provider,
                ToolBox::new(root.clone()),
                SkillRegistry::discover(&root),
                session,
            )?;
            let catalog = match agent.fetch_models().await {
                Ok(catalog) => {
                    agent.resolve_auto_model(&catalog);
                    catalog
                }
                Err(error) => {
                    catalog_error = Some(format!("Could not load the model catalog: {error:#}"));
                    Vec::new()
                }
            };
            self.model_catalogs.insert(id, catalog);
            self.conversations.install(agent)?
        };
        self.model_catalog = self.model_catalogs.get(&id).cloned().unwrap_or_default();
        self.sync_active_session();
        self.restore_turn_state(id);
        if catalog_error.is_some() {
            self.error = catalog_error;
        }
        self.session_picker = None;
        if !self.is_running() && self.active_goal_id().is_some() {
            self.start_goal_continuation()?;
        }
        Ok(())
    }

    fn park_current_turn(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        let event_rx = self
            .event_rx
            .take()
            .expect("a running terminal turn has an event stream");
        let id = self.conversation.snapshot().session.id;
        self.background_turns.insert(
            id,
            BackgroundTurn {
                running,
                event_rx,
                pending_user: self.pending_user.take(),
                queued_prompt: self.queued_prompt.take(),
                pending_goal_action: self.pending_goal_action.take(),
                live_messages: std::mem::take(&mut self.live_messages),
            },
        );
    }

    fn restore_turn_state(&mut self, id: Uuid) {
        let Some(background) = self.background_turns.remove(&id) else {
            return;
        };
        self.running = Some(background.running);
        self.event_rx = Some(background.event_rx);
        self.pending_user = background.pending_user;
        self.queued_prompt = background.queued_prompt;
        self.pending_goal_action = background.pending_goal_action;
        self.live_messages = background.live_messages;
    }

    async fn delete_session_selection(&mut self) -> Result<()> {
        let Some(SessionPickerRow::Session(project_index, session_index)) = self
            .session_picker
            .as_ref()
            .and_then(SessionPicker::selected_row)
        else {
            return Ok(());
        };
        let selected = self.session_picker.as_ref().unwrap().selected;
        let (root, id) = {
            let picker = self.session_picker.as_ref().unwrap();
            let project = &picker.projects[project_index].project;
            (project.root.clone(), project.sessions[session_index].id)
        };
        let store = SessionStore::new(&root)?;
        let active_snapshot = self.conversation.snapshot();
        let deleting_active =
            id == active_snapshot.session.id && paths_equal(&root, &active_snapshot.project_root);
        if let Some(conversation) = self.conversations.take_if_idle(id)? {
            conversation.shutdown().await?;
        }
        self.background_turns.remove(&id);
        self.model_catalogs.remove(&id);
        store.delete(&id.to_string())?;

        if deleting_active {
            let mut session =
                store.create_for_provider(active_snapshot.session.provider, self.model.clone())?;
            session.reasoning_effort.clone_from(&self.reasoning_effort);
            session.service_tier.clone_from(&self.service_tier);
            let mut provider = OpenAiCompatible::new(&self.config, &session.provider)?;
            provider.set_debug_openai(self.debug_openai);
            let agent = Agent::new(
                provider,
                ToolBox::new(root.clone()),
                SkillRegistry::discover(&root),
                session,
            )?;
            self.conversation = self.conversations.install(agent)?;
            self.model_catalogs.insert(
                self.conversation.snapshot().session.id,
                self.model_catalog.clone(),
            );
            self.sync_active_session();
        }

        if store.list()?.is_empty() {
            self.registry.unregister(&root)?;
        }
        let projects = list_session_projects(&self.project_root, &self.registry)?;
        let expanded = self
            .session_picker
            .as_ref()
            .map(|picker| {
                picker
                    .projects
                    .iter()
                    .filter(|project| project.expanded)
                    .map(|project| project.project.root.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut picker = SessionPicker {
            projects: projects
                .into_iter()
                .map(|project| SessionProjectView {
                    expanded: expanded.iter().any(|root| paths_equal(root, &project.root)),
                    project,
                })
                .collect(),
            selected: 0,
        };
        picker.selected = selected.min(picker.rows().len().saturating_sub(1));
        self.session_picker = Some(picker);
        self.error = None;
        Ok(())
    }

    fn sync_active_session(&mut self) {
        self.apply_snapshot(self.conversation.snapshot());
        self.live_messages.clear();
        self.pending_user = None;
        self.queued_prompt = None;
        self.goal_picker = None;
        self.pending_goal_action = None;
        self.editing_goal_id = None;
        self.editing_goal_resume = false;
        self.goal_buttons = GoalButtons::default();
        self.last_escape = None;
        self.steer_button = None;
        self.conversation_view = None;
        self.text_selection = None;
        self.copy_flash = None;
        self.error = None;
        self.scroll = 0;
        self.auto_scroll = true;
    }

    fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
        self.auto_scroll = false;
    }

    fn scroll_down(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_add(amount).min(self.max_scroll);
        self.auto_scroll = self.scroll >= self.max_scroll;
    }

    fn drain_agent_events(&mut self) {
        let mut pending = Vec::new();
        if let Some(receiver) = &mut self.event_rx {
            while let Ok(event) = receiver.try_recv() {
                pending.push(event);
            }
        }
        for event in pending {
            match event {
                AgentEvent::UserMessage(_) => {}
                AgentEvent::AssistantMessage(message) => self.live_messages.push(message),
                AgentEvent::AssistantTextDelta {
                    delta,
                    start,
                    sequence,
                    created_at,
                } => {
                    if start {
                        let mut message = Message::text(Role::Assistant, delta);
                        message.sequence = Some(sequence);
                        message.created_at = Some(created_at);
                        self.live_messages.push(message);
                    } else if let Some(message) = self.live_messages.last_mut() {
                        message.content.get_or_insert_default().push_str(&delta);
                    }
                }
                AgentEvent::AssistantStreamReset => {
                    if self
                        .live_messages
                        .last()
                        .is_some_and(|message| matches!(message.role, Role::Assistant))
                    {
                        self.live_messages.pop();
                    }
                }
                AgentEvent::AssistantMessageCompleted(message) => {
                    if let Some(existing) = self
                        .live_messages
                        .iter_mut()
                        .rfind(|existing| matches!(existing.role, Role::Assistant))
                    {
                        *existing = message;
                    } else {
                        self.live_messages.push(message);
                    }
                }
                AgentEvent::Activity(activity) => {
                    if let Some(existing) = self
                        .activities
                        .iter_mut()
                        .find(|existing| existing.id == activity.id)
                    {
                        existing.clone_from(&activity);
                    } else {
                        self.activities.push(activity);
                    }
                }
            }
        }
    }

    fn copy_to_clipboard(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let result = self
            .clipboard
            .as_mut()
            .context("the operating-system clipboard is unavailable")
            .and_then(|clipboard| clipboard.set_text(text).map_err(Into::into));
        if let Err(error) = result {
            self.error = Some(format!("Could not copy to the clipboard: {error}"));
        }
    }

    fn update_selection_cursor(&mut self, column: u16, row: u16) {
        let point = self
            .conversation_view
            .as_ref()
            .and_then(|view| view.point_at(column, row, true));
        if let (Some(selection), Some(point)) = (&mut self.text_selection, point) {
            selection.moved |= point != selection.anchor;
            selection.cursor = point;
            selection.last_column = column;
            selection.last_row = row;
        }
    }

    fn finish_text_selection(&mut self, column: u16, row: u16) {
        self.update_selection_cursor(column, row);
        let text = self.text_selection.as_mut().and_then(|selection| {
            selection.dragging = false;
            selection
                .moved
                .then(|| {
                    self.conversation_view
                        .as_ref()
                        .map(|view| view.selected_text(selection))
                })
                .flatten()
        });
        if let Some(text) = text {
            self.copy_to_clipboard(text);
        } else {
            self.text_selection = None;
        }
    }

    fn update_drag_autoscroll(&mut self) {
        if self
            .copy_flash
            .as_ref()
            .is_some_and(|flash| Instant::now() >= flash.until)
        {
            self.copy_flash = None;
        }
        let Some(selection) = self
            .text_selection
            .as_ref()
            .filter(|selection| selection.dragging)
        else {
            return;
        };
        let Some(view) = &self.conversation_view else {
            return;
        };
        let column = selection.last_column;
        let row = selection.last_row;
        let (direction, amount) = if row < view.area.y {
            (-1, view.area.y.saturating_sub(row).clamp(1, 5))
        } else if row >= view.area.bottom() {
            (
                1,
                row.saturating_sub(view.area.bottom())
                    .saturating_add(1)
                    .clamp(1, 5),
            )
        } else {
            return;
        };
        if direction < 0 {
            self.scroll_up(amount);
        } else {
            self.scroll_down(amount);
        }
        if let Some(view) = &mut self.conversation_view {
            view.scroll = self.scroll as usize;
        }
        self.update_selection_cursor(column, row);
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.goal_picker.is_none()
            && self.session_picker.is_none()
            && self.model_picker.is_none()
            && !self.show_help
            && !self.show_skills
        {
            let point = (mouse.column, mouse.row).into();
            if self
                .goal_buttons
                .list
                .is_some_and(|area| area.contains(point))
            {
                self.open_goal_picker();
                return;
            }
            if let Some(id) = self.visible_goal_id {
                if self
                    .goal_buttons
                    .toggle
                    .is_some_and(|area| area.contains(point))
                {
                    self.request_goal_action(PendingGoalAction::Toggle(id));
                    return;
                }
                if self
                    .goal_buttons
                    .edit
                    .is_some_and(|area| area.contains(point))
                {
                    self.request_goal_action(PendingGoalAction::BeginEdit(id));
                    return;
                }
                if self
                    .goal_buttons
                    .delete
                    .is_some_and(|area| area.contains(point))
                {
                    self.request_goal_action(PendingGoalAction::Delete(id));
                    return;
                }
            }
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.queued_prompt.is_some()
            && self
                .steer_button
                .is_some_and(|area| area.contains((mouse.column, mouse.row).into()))
        {
            self.cancel_current_turn();
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Right) => {
                let target = self.conversation_view.as_ref().and_then(|view| {
                    view.point_at(mouse.column, mouse.row, false)
                        .and_then(|point| view.copy_target_at(point))
                        .map(|target| (target.text.clone(), target.ranges.clone()))
                });
                if let Some((text, ranges)) = target {
                    self.text_selection = None;
                    self.copy_flash = Some(CopyFlash {
                        ranges,
                        until: Instant::now() + Duration::from_millis(500),
                    });
                    self.copy_to_clipboard(text);
                }
                return;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let turn_toggle = self.conversation_view.as_ref().and_then(|view| {
                    let point = view.point_at(mouse.column, mouse.row, false)?;
                    Some((
                        view.turn_toggle_at(point)?,
                        point.row.saturating_sub(view.scroll),
                    ))
                });
                if let Some((key, viewport_row)) = turn_toggle {
                    if !self.expanded_turns.remove(&key) {
                        self.expanded_turns.insert(key);
                    }
                    self.pending_turn_anchor = Some(TurnAnchor { key, viewport_row });
                    self.text_selection = None;
                    self.copy_flash = None;
                    self.auto_scroll = false;
                    return;
                }
                if let Some(point) = self
                    .conversation_view
                    .as_ref()
                    .and_then(|view| view.point_at(mouse.column, mouse.row, false))
                {
                    self.copy_flash = None;
                    self.auto_scroll = false;
                    self.text_selection = Some(TextSelection {
                        anchor: point,
                        cursor: point,
                        dragging: true,
                        moved: false,
                        last_column: mouse.column,
                        last_row: mouse.row,
                    });
                    return;
                }
            }
            MouseEventKind::Drag(MouseButton::Left)
                if self
                    .text_selection
                    .as_ref()
                    .is_some_and(|selection| selection.dragging) =>
            {
                self.update_selection_cursor(mouse.column, mouse.row);
                return;
            }
            MouseEventKind::Up(MouseButton::Left)
                if self
                    .text_selection
                    .as_ref()
                    .is_some_and(|selection| selection.dragging) =>
            {
                self.finish_text_selection(mouse.column, mouse.row);
                return;
            }
            _ => {}
        }
        match mouse.kind {
            MouseEventKind::ScrollUp if self.show_skills => {
                self.move_skill_selection(-1);
            }
            MouseEventKind::ScrollDown if self.show_skills => {
                self.move_skill_selection(1);
            }
            MouseEventKind::ScrollUp if self.model_picker.is_some() => {
                self.move_model_selection(-1);
            }
            MouseEventKind::ScrollDown if self.model_picker.is_some() => {
                self.move_model_selection(1);
            }
            MouseEventKind::ScrollUp if self.session_picker.is_some() => {
                self.move_session_selection(-1);
            }
            MouseEventKind::ScrollDown if self.session_picker.is_some() => {
                self.move_session_selection(1);
            }
            MouseEventKind::ScrollUp if self.goal_picker.is_some() => {
                self.move_goal_selection(-1);
            }
            MouseEventKind::ScrollDown if self.goal_picker.is_some() => {
                self.move_goal_selection(1);
            }
            MouseEventKind::ScrollUp if self.completion.is_some() => {
                self.move_completion(-1);
            }
            MouseEventKind::ScrollDown if self.completion.is_some() => {
                self.move_completion(1);
            }
            MouseEventKind::ScrollUp => self.scroll_up(3),
            MouseEventKind::ScrollDown => self.scroll_down(3),
            _ => {}
        }
    }

    async fn finish_turn_if_ready(&mut self) -> Result<()> {
        if !self.running.as_ref().is_some_and(JoinHandle::is_finished) {
            return Ok(());
        }
        let handle = self.running.take().expect("checked above");
        let turn = handle.await.context("conversation turn task failed")??;
        self.apply_snapshot(turn.snapshot);
        self.event_rx = None;
        self.last_escape = None;
        self.live_messages.clear();
        self.pending_user = None;
        let turn_succeeded = match turn.result {
            Ok(_) => {
                self.error = None;
                true
            }
            Err(error) if turn_was_cancelled(&error) => {
                self.error = None;
                false
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                false
            }
        };
        if self.apply_pending_goal_action().await? {
            return Ok(());
        }
        if let Some(prompt) = self.queued_prompt.take() {
            self.start_turn(prompt, false)?;
        } else if turn_succeeded && self.active_goal_id().is_some() {
            self.start_goal_continuation()?;
        }
        Ok(())
    }

    async fn submit(&mut self) -> Result<()> {
        if self.recording.is_some() || self.transcription.is_some() {
            return Ok(());
        }
        let prompt = self.input.trim().to_owned();
        if prompt.is_empty() {
            return Ok(());
        }
        if let Some(id) = self.editing_goal_id.take() {
            if prompt.chars().count() > 4_000 {
                self.editing_goal_id = Some(id);
                self.error = Some("Goal objective cannot exceed 4,000 characters.".into());
                return Ok(());
            }
            let resume = std::mem::take(&mut self.editing_goal_resume);
            if let Some(snapshot) = self.conversation.edit_goal(id, prompt, resume).await? {
                self.input.clear();
                self.cursor = 0;
                self.preferred_column = None;
                self.close_completion();
                self.apply_snapshot(snapshot);
                self.error = None;
                if resume {
                    self.start_goal_continuation()?;
                }
            } else {
                self.error = Some("The goal no longer exists.".into());
            }
            return Ok(());
        }
        if prompt == "/goal" {
            self.error = Some("Write the objective after /goal.".into());
            return Ok(());
        }
        if let Some(objective) = goal_objective_from_input(&self.input).map(str::to_owned) {
            self.input.clear();
            self.cursor = 0;
            self.preferred_column = None;
            self.close_completion();
            self.request_goal_action(PendingGoalAction::Create(objective));
            if !self.is_running() {
                self.apply_pending_goal_action().await?;
            }
            return Ok(());
        }
        if prompt == "/goals" {
            self.input.clear();
            self.cursor = 0;
            self.preferred_column = None;
            self.close_completion();
            return self.command(&prompt).await;
        }
        if prompt == "/providers" || prompt.starts_with("/provider ") {
            self.input.clear();
            self.cursor = 0;
            self.preferred_column = None;
            self.close_completion();
            return self.provider_command(&prompt);
        }
        if self.is_running() {
            if self.queued_prompt.is_none() {
                self.input.clear();
                self.cursor = 0;
                self.preferred_column = None;
                self.close_completion();
                self.queued_prompt = Some(prompt);
            }
            return Ok(());
        }
        if builtin_command_from_input(&self.input).is_some() {
            self.input.clear();
            self.cursor = 0;
            self.preferred_column = None;
            self.close_completion();
            return self.command(&prompt).await;
        }

        self.start_turn(prompt, true)
    }

    fn start_turn(&mut self, prompt: String, clear_composer: bool) -> Result<()> {
        if clear_composer {
            self.input.clear();
            self.cursor = 0;
            self.preferred_column = None;
            self.close_completion();
        }
        self.error = None;
        self.pending_user = Some(prompt.clone());
        self.live_messages.clear();

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_rx = Some(event_rx);
        self.running = Some(self.conversation.start_turn(prompt, Some(event_tx))?);
        Ok(())
    }

    fn start_goal_continuation(&mut self) -> Result<()> {
        self.error = None;
        self.pending_user = None;
        self.live_messages.clear();

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_rx = Some(event_rx);
        self.running = Some(self.conversation.start_goal_continuation(Some(event_tx))?);
        Ok(())
    }

    fn cancel_current_turn(&mut self) {
        self.conversation.cancel();
    }

    fn provider_command(&mut self, command: &str) -> Result<()> {
        if self.is_running() {
            self.error = Some("Wait for the active turn before changing providers.".into());
            return Ok(());
        }
        let parts = command.split_whitespace().collect::<Vec<_>>();
        let store = ConfigStore::global()?;
        let mut persisted = store.load()?;
        match parts.as_slice() {
            ["/providers"] => {
                self.error = Some(
                    persisted
                        .summaries()
                        .into_iter()
                        .map(|provider| {
                            format!(
                                "{}{}: {} · {} · key {}",
                                if provider.active { "* " } else { "" },
                                provider.name,
                                provider.model,
                                provider.base_url,
                                if provider.api_key_configured {
                                    "configured"
                                } else {
                                    "none"
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            ["/provider", "use", name] => {
                persisted.provider(name)?;
                persisted.active_provider = (*name).into();
                store.save(&persisted)?;
                self.config = persisted.clone();
                self.error = Some(format!(
                    "Provider {name:?} selected for new sessions. The current session keeps provider {:?}.",
                    self.conversation.snapshot().session.provider
                ));
            }
            ["/provider", "remove", name] => {
                persisted.provider(name)?;
                if persisted.active_provider == *name {
                    anyhow::bail!("select another active provider before removing this one");
                }
                persisted.providers.remove(*name);
                store.save(&persisted)?;
                self.config = persisted.clone();
                self.error = Some(format!("Provider {name:?} removed."));
            }
            [
                "/provider",
                "add",
                name,
                base_url,
                model,
                auth,
                api_key @ ..,
            ] if !api_key.is_empty() => {
                validate_provider_name(name)?;
                let provider = ProviderConfig {
                    model: (*model).into(),
                    base_url: (*base_url).into(),
                    auth: (*auth).into(),
                    api_key: api_key.join(" "),
                    ..persisted.providers.get(*name).cloned().unwrap_or_default()
                };
                provider.validate(name)?;
                persisted.providers.insert((*name).into(), provider);
                store.save(&persisted)?;
                self.config = persisted.clone();
                self.error = Some(format!("Provider {name:?} saved."));
            }
            _ => {
                self.error = Some(
                    "Usage: /providers | /provider use NAME | /provider remove NAME | /provider add NAME BASE_URL MODEL AUTH API_KEY"
                        .into(),
                );
            }
        }
        Ok(())
    }

    async fn command(&mut self, command: &str) -> Result<()> {
        match command {
            "/quit" => self.should_quit = true,
            "/help" => self.show_help = true,
            "/model" | "/models" => self.open_model_picker(),
            "/skills" => self.open_skill_picker(),
            "/sessions" => self.open_session_picker().await?,
            "/branches" => self.open_branch_navigator(),
            "/goals" => self.open_goal_picker(),
            "/clear" => {
                let snapshot = self.conversation.clear().await?;
                self.apply_snapshot(snapshot);
                self.live_messages.clear();
                self.error = None;
            }
            _ => self.error = Some(format!("Unknown command: {command}")),
        }
        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(());
        }
        if self.branch_navigator.is_some() {
            match key.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                    self.move_branch_selection(key.code)?
                }
                KeyCode::Enter => self.confirm_branch_selection().await?,
                KeyCode::Esc => self.cancel_branch_navigator(),
                _ => {}
            }
            return Ok(());
        }
        if self.goal_picker.is_some() {
            let describing = self
                .goal_picker
                .as_ref()
                .is_some_and(|picker| picker.describing);
            match key.code {
                KeyCode::Up => self.move_goal_selection(-1),
                KeyCode::Down => self.move_goal_selection(1),
                KeyCode::PageUp => self.move_goal_selection(if describing { -8 } else { -5 }),
                KeyCode::PageDown => self.move_goal_selection(if describing { 8 } else { 5 }),
                KeyCode::Char('d' | 'D') => self.describe_selected_goal(),
                KeyCode::Enter | KeyCode::Char(' ') if !describing => self.toggle_selected_goal(),
                KeyCode::Delete | KeyCode::Backspace if !describing => self.delete_selected_goal(),
                KeyCode::Esc if describing => self.describe_selected_goal(),
                KeyCode::Esc => self.goal_picker = None,
                _ => {}
            }
            return Ok(());
        }
        if self.is_running() {
            if key.code == KeyCode::Esc {
                if key.kind == KeyEventKind::Repeat {
                    return Ok(());
                }
                let now = Instant::now();
                if self
                    .last_escape
                    .is_some_and(|previous| now.duration_since(previous) < Duration::from_secs(1))
                {
                    self.last_escape = None;
                    self.request_stop();
                } else {
                    self.last_escape = Some(now);
                    self.close_completion();
                }
                return Ok(());
            }
            self.last_escape = None;
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
                KeyCode::Enter | KeyCode::Tab => self.accept_model_selection().await?,
                KeyCode::Left | KeyCode::Backspace | KeyCode::Esc => self.back_model_picker(),
                _ => {}
            }
            return Ok(());
        }
        if self.session_picker.is_some() {
            match key.code {
                KeyCode::Up => self.move_session_selection(-1),
                KeyCode::Down => self.move_session_selection(1),
                KeyCode::PageUp => self.move_session_selection(-5),
                KeyCode::PageDown => self.move_session_selection(5),
                KeyCode::Left => self.move_session_left(),
                KeyCode::Right => self.move_session_right(),
                KeyCode::Enter => self.accept_session_selection().await?,
                KeyCode::Delete | KeyCode::Backspace => self.delete_session_selection().await?,
                KeyCode::Esc => self.session_picker = None,
                _ => {}
            }
            return Ok(());
        }

        if self.editing_goal_id.is_some() && key.code == KeyCode::Esc {
            let id = self.editing_goal_id.take().expect("checked above");
            let resume = std::mem::take(&mut self.editing_goal_resume);
            self.input.clear();
            self.cursor = 0;
            self.preferred_column = None;
            self.close_completion();
            self.error = None;
            if resume && let Some(snapshot) = self.conversation.activate_goal(id).await? {
                self.apply_snapshot(snapshot);
                self.start_goal_continuation()?;
            }
            return Ok(());
        }

        if is_dictation_shortcut(&key) {
            if let Err(error) = self.toggle_dictation() {
                self.error = Some(format!("Dictation failed: {error:#}"));
            }
            return Ok(());
        }

        if self.recording.is_some() {
            if key.code == KeyCode::Enter
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT)
                && let Err(error) = self.stop_dictation(true)
            {
                self.error = Some(format!("Dictation failed: {error:#}"));
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
                    self.close_completion();
                    return Ok(());
                }
                _ => {}
            }
        }

        if let Some(action) = editor_action(&key) {
            self.apply_editor_action(action);
            return Ok(());
        }

        if key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Char('c' | 'd') if !self.is_busy() => self.should_quit = true,
                KeyCode::Char('c') => {}
                KeyCode::Char('j') => self.insert("\n"),
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
            KeyCode::Enter => self.submit().await?,
            KeyCode::Char('?') if self.input.is_empty() => self.show_help = true,
            KeyCode::Char(value) => self.insert_char(value),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
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
                self.close_completion();
            }
            _ => {}
        }
        Ok(())
    }
}

pub(crate) async fn interactive(
    mut agent: Agent,
    registry: &SessionRegistry,
    debug_openai: bool,
    config: Config,
) -> Result<()> {
    let (model_catalog, catalog_error) = match agent.fetch_models().await {
        Ok(catalog) => {
            agent.resolve_auto_model(&catalog);
            (catalog, None)
        }
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    enable_raw_mode()?;
    let keyboard_enhancement =
        crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    if keyboard_enhancement {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = run_tui(
        &mut terminal,
        App::new(
            agent,
            model_catalog,
            catalog_error,
            debug_openai,
            config,
            registry.clone(),
        )?,
    )
    .await;

    if keyboard_enhancement {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
    }
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
) -> Result<String> {
    if app.active_goal_id().is_some() {
        app.start_goal_continuation()?;
    }
    loop {
        app.drain_agent_events();
        app.drain_completion_updates();
        app.finish_turn_if_ready().await?;
        if !app.is_running() {
            app.apply_pending_goal_action().await?;
        }
        app.finish_transcription_if_ready().await?;
        app.update_drag_autoscroll();
        app.spinner = app.spinner.wrapping_add(1);
        terminal.draw(|frame| render(frame, &mut app))?;

        if app.should_quit && !app.is_busy() {
            break;
        }
        if event::poll(Duration::from_millis(70))? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key).await?,
                Event::Paste(text) => {
                    app.handle_paste(&text);
                }
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                _ => {}
            }
        }
    }

    let session_id = app.conversation.snapshot().session.id;
    app.conversations.cancel_all();
    while app
        .conversations
        .statuses()
        .iter()
        .any(|status| status.lifecycle == ConversationLifecycle::Running)
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    app.conversations.shutdown_all().await?;
    Ok(session_id.to_string())
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    app.composer_width = area.width.saturating_sub(2).max(1) as usize;
    let input_lines = composer_rows(&app.input, app.composer_width)
        .len()
        .clamp(1, 6) as u16;
    let queued_height = if app.queued_prompt.is_some() { 3 } else { 0 };
    let goal_height = if app.visible_goal().is_some() { 3 } else { 0 };
    let [header, body, queued, goal, composer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(queued_height),
        Constraint::Length(goal_height),
        Constraint::Length(input_lines + 2),
    ])
    .areas(area);

    render_header(frame, app, header);
    if app.branch_navigator.is_some() {
        let panel_width = (body.width / 4).clamp(8, 22);
        let [chat, branches] = Layout::horizontal([
            Constraint::Min(body.width.saturating_sub(panel_width)),
            Constraint::Length(panel_width),
        ])
        .areas(body);
        render_chat(frame, app, chat);
        render_branch_navigator(frame, app, branches);
    } else {
        render_chat(frame, app, body);
    }
    render_queued_prompt(frame, app, queued);
    render_goal(frame, app, goal);
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
    if app.session_picker.is_some() {
        render_session_picker(frame, app, area);
    }
    if app.goal_picker.is_some() {
        render_goal_picker(frame, app, area);
    }
}

fn render_queued_prompt(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(prompt) = &app.queued_prompt else {
        app.steer_button = None;
        return;
    };
    if area.width < 16 || area.height < 3 {
        app.steer_button = None;
        return;
    }

    let button_width = 9.min(area.width);
    let message_width = area.width.saturating_sub(button_width);
    let [message, button] = Layout::horizontal([
        Constraint::Length(message_width),
        Constraint::Length(button_width),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" QUEUED  ", Style::default().fg(MUTED)),
            Span::raw(prompt.clone()),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED)),
        )
        .wrap(Wrap { trim: true }),
        message,
    );
    frame.render_widget(
        Paragraph::new("Steer").centered().block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(CRAB)),
        ),
        button,
    );
    app.steer_button = Some(button);
}

fn render_goal(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(goal) = app.visible_goal().cloned() else {
        app.goal_buttons = GoalButtons::default();
        return;
    };
    if area.height < 3 {
        app.goal_buttons = GoalButtons::default();
        return;
    }
    if area.width < 34 {
        frame.render_widget(
            Paragraph::new(compact_text(
                &format!(
                    "{}  {}",
                    match goal.status {
                        GoalStatus::Active => "ACTIVE",
                        GoalStatus::Paused => "PAUSED",
                        GoalStatus::Completed => "DONE",
                        GoalStatus::Blocked => "BLOCKED",
                    },
                    goal.objective.replace('\n', " ")
                ),
                area.width.saturating_sub(2) as usize,
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(GOAL)),
            ),
            area,
        );
        app.goal_buttons = GoalButtons::default();
        return;
    }

    let widths = [7, 6, 8, 7];
    let controls_width = widths.iter().sum::<u16>();
    let message_width = area.width.saturating_sub(controls_width);
    let [message, toggle, edit, delete, list] = Layout::horizontal([
        Constraint::Length(message_width),
        Constraint::Length(widths[0]),
        Constraint::Length(widths[1]),
        Constraint::Length(widths[2]),
        Constraint::Length(widths[3]),
    ])
    .areas(area);
    let status = match goal.status {
        GoalStatus::Active => "ACTIVE",
        GoalStatus::Paused => "PAUSED",
        GoalStatus::Completed => "DONE",
        GoalStatus::Blocked => "BLOCKED",
    };
    let status_color = match goal.status {
        GoalStatus::Active => GOAL,
        GoalStatus::Paused => Color::Yellow,
        GoalStatus::Completed => AQUA,
        GoalStatus::Blocked => Color::Red,
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {status}  "),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(goal.objective.replace('\n', " ")),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(status_color)),
        ),
        message,
    );
    let button = |label: &str, color: Color| {
        Paragraph::new(label.to_owned()).centered().block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color)),
        )
    };
    frame.render_widget(
        button(
            if goal.status == GoalStatus::Active {
                "Pause"
            } else {
                "Play"
            },
            status_color,
        ),
        toggle,
    );
    frame.render_widget(button("Edit", MUTED), edit);
    frame.render_widget(button("Delete", Color::Red), delete);
    frame.render_widget(button("Goals", GOAL), list);
    app.goal_buttons = GoalButtons {
        toggle: Some(toggle),
        edit: Some(edit),
        delete: Some(delete),
        list: Some(list),
    };
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
    let status_color = if app.recording.is_some() {
        Color::Red
    } else if app.is_running() || app.transcription.is_some() {
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
        + separator.chars().count() * 3
        + if fast { 2 } else { 0 };
    let mut spans = vec![
        Span::styled(status, Style::default().fg(status_color)),
        Span::styled("  │  ", Style::default().fg(MUTED)),
        Span::styled(&app.model, Style::default().fg(Color::White)),
    ];
    if fast {
        spans.push(Span::styled(" ⚡", Style::default().fg(Color::Yellow)));
    }
    spans.extend([
        Span::styled(separator, Style::default().fg(MUTED)),
        Span::styled(thinking, Style::default().fg(AQUA)),
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
    let inner = area.inner(Margin {
        horizontal: 0,
        vertical: 1,
    });

    let source = conversation_source(app);
    let rows = wrap_conversation_lines(&source.lines, inner.width.max(1));
    if let Some(anchor) = app.pending_turn_anchor.take()
        && let Some(summary_line) = source
            .turn_toggles
            .iter()
            .find(|target| target.key == anchor.key)
            .map(|target| target.line)
        && let Some(summary_row) = rows.iter().position(|row| row.source_line == summary_line)
    {
        app.scroll = summary_row
            .saturating_sub(anchor.viewport_row)
            .min(u16::MAX as usize) as u16;
    }
    if let Some(node_id) = app.pending_branch_node.take()
        && let Some(source_line) = source
            .node_lines
            .iter()
            .find_map(|(id, line)| (*id == node_id).then_some(*line))
        && let Some(node_row) = rows.iter().position(|row| row.source_line == source_line)
    {
        app.scroll = node_row.saturating_sub(2).min(u16::MAX as usize) as u16;
    }
    app.max_scroll = rows
        .len()
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    if app.auto_scroll {
        app.scroll = app.max_scroll;
    } else {
        app.scroll = app.scroll.min(app.max_scroll);
    }
    let visible = rows
        .iter()
        .enumerate()
        .skip(app.scroll as usize)
        .take(inner.height as usize)
        .map(|(row_index, row)| visual_row_line(app, row_index, row))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), inner);
    app.conversation_view = Some(ConversationView {
        area: inner,
        rows,
        scroll: app.scroll as usize,
        copy_targets: source.copy_targets,
        turn_toggles: source.turn_toggles,
    });
}

#[cfg(test)]
fn conversation_lines(app: &App) -> Vec<Line<'static>> {
    conversation_source(app).lines
}

fn conversation_source(app: &App) -> ConversationSource {
    let mut source = ConversationSource {
        lines: Vec::new(),
        copy_targets: Vec::new(),
        turn_toggles: Vec::new(),
        node_lines: Vec::new(),
    };
    let mut message_index = 0;
    while message_index < app.transcript.len() {
        let message = &app.transcript[message_index];
        if !matches!(message.role, Role::User) {
            if matches!(message.role, Role::Assistant)
                && let Some(content) = visible_message_content(message)
            {
                push_actor_label(&mut source, "CRAB", CRAB);
                push_message_lines(&mut source, content, true, Some(&app.markdown));
                source.lines.push(Line::default());
            }
            message_index += 1;
            continue;
        }

        if let Some(content) = visible_message_content(message) {
            if let Some(node_id) = app.transcript_node_ids.get(message_index).copied() {
                source.node_lines.push((node_id, source.lines.len()));
            }
            push_actor_label(&mut source, "USER", AQUA);
            push_message_lines(&mut source, content, true, None);
            source.lines.push(Line::default());
        }

        let next_user = app.transcript[message_index + 1..]
            .iter()
            .position(|candidate| matches!(candidate.role, Role::User))
            .map(|offset| message_index + 1 + offset)
            .unwrap_or(app.transcript.len());
        let turn_messages = &app.transcript[message_index + 1..next_user];
        if turn_has_visible_content(app, message_index, turn_messages) {
            push_actor_label(&mut source, "CRAB", CRAB);
            push_turn_lines(&mut source, app, message_index, turn_messages);
            source.lines.push(Line::default());
        }
        message_index = next_user;
    }

    if let Some(content) = &app.pending_user {
        push_actor_label(&mut source, "USER", AQUA);
        push_message_lines(&mut source, content, true, None);
        source.lines.push(Line::default());
        let turn_message_index = app.transcript.len();
        if turn_has_visible_content(app, turn_message_index, &app.live_messages) {
            push_actor_label(&mut source, "CRAB", CRAB);
            push_turn_lines(&mut source, app, turn_message_index, &app.live_messages);
            source.lines.push(Line::default());
        }
    }
    if let Some(error) = &app.error {
        source.lines.push(Line::from(Span::styled(
            " ERROR ",
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        )));
        source.lines.extend(error.lines().map(|line| {
            Line::from(Span::styled(
                line.to_owned(),
                Style::default().fg(Color::Red),
            ))
        }));
    }
    source
}

fn branch_rows(nodes: &[ConversationGraphNode]) -> Vec<BranchRow> {
    let ids = nodes.iter().map(|node| node.id).collect::<HashSet<_>>();
    let mut children = HashMap::<Option<Uuid>, Vec<Uuid>>::new();
    for node in nodes {
        let parent = node.parent_id.filter(|parent| ids.contains(parent));
        children.entry(parent).or_default().push(node.id);
    }
    fn visit(
        id: Uuid,
        depth: usize,
        ancestor_continues: Vec<bool>,
        is_last: bool,
        children: &HashMap<Option<Uuid>, Vec<Uuid>>,
        rows: &mut Vec<BranchRow>,
    ) {
        rows.push(BranchRow {
            id,
            depth,
            ancestor_continues: ancestor_continues.clone(),
            is_last,
        });
        let descendants = children.get(&Some(id)).map(Vec::as_slice).unwrap_or(&[]);
        for (index, child) in descendants.iter().enumerate() {
            let mut continues = ancestor_continues.clone();
            if depth > 0 {
                continues.push(!is_last);
            }
            visit(
                *child,
                depth + 1,
                continues,
                index + 1 == descendants.len(),
                children,
                rows,
            );
        }
    }
    let mut rows = Vec::new();
    let roots = children.get(&None).map(Vec::as_slice).unwrap_or(&[]);
    for (index, root) in roots.iter().enumerate() {
        visit(
            *root,
            0,
            Vec::new(),
            index + 1 == roots.len(),
            &children,
            &mut rows,
        );
    }
    rows
}

fn render_branch_navigator(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(navigator) = &mut app.branch_navigator else {
        return;
    };
    if area.height < 3 {
        return;
    }
    let body_height = area.height.saturating_sub(2) as usize;
    let selected_row = navigator
        .rows
        .iter()
        .position(|row| row.id == navigator.nodes[navigator.selected].id)
        .unwrap_or(0);
    if selected_row < navigator.offset {
        navigator.offset = selected_row;
    } else if selected_row >= navigator.offset.saturating_add(body_height) {
        navigator.offset = selected_row.saturating_sub(body_height.saturating_sub(1));
    }

    let mut lines = Vec::with_capacity(area.height as usize);
    lines.push(Line::from(Span::styled(
        " branches",
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
    )));
    for row in navigator
        .rows
        .iter()
        .skip(navigator.offset)
        .take(body_height)
    {
        let previewed = navigator.preview_path.contains(&row.id);
        let original = navigator.original_path.contains(&row.id);
        let color = if previewed {
            CRAB
        } else if original {
            AQUA
        } else {
            MUTED
        };
        let mut prefix = String::new();
        for continues in &row.ancestor_continues {
            prefix.push_str(if *continues { "│ " } else { "  " });
        }
        if row.depth > 0 {
            prefix.push_str(if previewed {
                if row.is_last { "┗━" } else { "┣━" }
            } else if row.is_last {
                "└─"
            } else {
                "├─"
            });
        }
        let selected = row.id == navigator.nodes[navigator.selected].id;
        prefix.push(if selected { '◉' } else { '●' });
        let mut style = Style::default().fg(color);
        if previewed {
            style = style.add_modifier(Modifier::BOLD);
        }
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        lines.push(Line::from(Span::styled(prefix, style)));
    }
    while lines.len() + 1 < area.height as usize {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        " arrows · ↵ · esc",
        Style::default().fg(MUTED),
    )));
    frame.render_widget(Paragraph::new(lines), area);
}

fn visible_message_content(message: &Message) -> Option<&str> {
    (!message.hidden)
        .then_some(message.content.as_deref())
        .flatten()
        .filter(|text| !text.trim().is_empty())
}

fn push_actor_label(source: &mut ConversationSource, label: &str, color: Color) {
    source.lines.push(Line::from(Span::styled(
        format!(" {label} "),
        Style::default()
            .fg(Color::Black)
            .bg(color)
            .add_modifier(Modifier::BOLD),
    )));
}

fn push_message_lines(
    source: &mut ConversationSource,
    content: &str,
    copyable: bool,
    markdown: Option<&MarkdownHighlighter>,
) {
    let mut ranges = Vec::new();
    let rendered = markdown
        .map(|highlighter| highlighter.render(content))
        .unwrap_or_else(|| {
            content
                .lines()
                .map(|line| Line::from(line.to_owned()))
                .collect()
        });
    for (line, rendered) in content.lines().zip(rendered) {
        let line_index = source.lines.len();
        source.lines.push(rendered);
        if !line.is_empty() {
            ranges.push(SourceRange {
                line: line_index,
                start: 0,
                end: line.len(),
            });
        }
    }
    if copyable && !ranges.is_empty() {
        source.copy_targets.push(CopyTarget {
            ranges,
            text: content.to_owned(),
        });
    }
}

fn turn_has_visible_content(app: &App, turn_message_index: usize, messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        matches!(message.role, Role::Assistant) && visible_message_content(message).is_some()
    }) || app
        .activities
        .iter()
        .any(|activity| activity.turn_message_index == turn_message_index)
}

fn push_turn_lines(
    source: &mut ConversationSource,
    app: &App,
    turn_message_index: usize,
    messages: &[Message],
) {
    let mut matched_activities = Vec::new();
    let mut events = Vec::new();
    for message in messages
        .iter()
        .filter(|message| matches!(message.role, Role::Assistant))
    {
        if visible_message_content(message).is_some() {
            events.push(TurnDisplayEvent::Message(message));
        }
        for call in message.tool_calls.iter().flatten() {
            if let Some(activity) = app.activities.iter().find(|activity| {
                activity.turn_message_index == turn_message_index && activity.id == call.id
            }) {
                events.push(TurnDisplayEvent::Activity(activity));
                matched_activities.push(activity.id.as_str());
            }
        }
    }
    for activity in app.activities.iter().filter(|activity| {
        activity.turn_message_index == turn_message_index
            && !matched_activities.contains(&activity.id.as_str())
    }) {
        events.push(TurnDisplayEvent::Activity(activity));
    }
    if events.iter().all(|event| event.sequence().is_some()) {
        events.sort_by_key(|event| event.sequence());
    }

    let completed_turn = app
        .turns
        .iter()
        .find(|turn| turn.message_index == turn_message_index && turn.completed_at.is_some());
    let final_message_index = events
        .iter()
        .rposition(|event| matches!(event, TurnDisplayEvent::Message(_)));
    let collapsible = completed_turn.is_some()
        && final_message_index.is_some_and(|index| index > 0 && index + 1 == events.len());
    if !collapsible {
        push_turn_events(source, app, &events);
        return;
    }

    let final_message_index = final_message_index.expect("collapsible turns have a final message");
    let key = TurnKey {
        session_id: app.session_id,
        message_index: turn_message_index,
    };
    let expanded = app.expanded_turns.contains(&key);
    if expanded {
        push_turn_events(source, app, &events[..final_message_index]);
        source.lines.push(Line::default());
    }
    let turn = completed_turn.expect("collapsible turns are completed");
    let operation_count = events[..final_message_index]
        .iter()
        .filter(|event| {
            matches!(
                event,
                TurnDisplayEvent::Activity(activity) if activity.tool != "model_request"
            )
        })
        .count();
    push_turn_summary(source, key, turn, operation_count, expanded);
    push_turn_events(source, app, &events[final_message_index..]);
}

fn push_turn_events(source: &mut ConversationSource, app: &App, events: &[TurnDisplayEvent<'_>]) {
    let mut emitted_any = false;
    let mut last_was_message = false;
    for event in events.iter().copied() {
        match event {
            TurnDisplayEvent::Message(message) => {
                let content =
                    visible_message_content(message).expect("visible messages were filtered");
                if emitted_any {
                    source.lines.push(Line::default());
                }
                push_message_lines(source, content, true, Some(&app.markdown));
                emitted_any = true;
                last_was_message = true;
            }
            TurnDisplayEvent::Activity(activity) => {
                if last_was_message {
                    source.lines.push(Line::default());
                }
                push_activity_line(source, app, activity);
                emitted_any = true;
                last_was_message = false;
            }
        }
    }
}

fn push_turn_summary(
    source: &mut ConversationSource,
    key: TurnKey,
    turn: &AgentTurn,
    operation_count: usize,
    expanded: bool,
) {
    let completed_at = turn
        .completed_at
        .expect("turn summaries require a completion time");
    let duration = completed_at
        .signed_duration_since(turn.started_at)
        .num_milliseconds()
        .max(0);
    let rounded_seconds = ((duration + 500) / 1_000).max(1);
    let duration = if rounded_seconds < 60 {
        format!("{rounded_seconds}s")
    } else {
        let minutes = rounded_seconds / 60;
        let seconds = rounded_seconds % 60;
        if seconds == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m {seconds}s")
        }
    };
    let operations = if operation_count == 1 {
        "1 operation".to_owned()
    } else {
        format!("{operation_count} operations")
    };
    let marker = if expanded { "▾" } else { "▸" };
    let line = source.lines.len();
    source.lines.push(Line::from(Span::styled(
        format!("  {marker} Worked for {duration} · {operations}"),
        Style::default().fg(MUTED),
    )));
    source.turn_toggles.push(TurnToggleTarget { line, key });
}

#[derive(Clone, Copy)]
enum TurnDisplayEvent<'a> {
    Message(&'a Message),
    Activity(&'a AgentActivity),
}

impl TurnDisplayEvent<'_> {
    fn sequence(self) -> Option<u64> {
        match self {
            Self::Message(message) => message.sequence,
            Self::Activity(activity) => activity.sequence,
        }
    }
}

fn push_activity_line(source: &mut ConversationSource, app: &App, activity: &AgentActivity) {
    let color = match activity.kind {
        ActivityKind::Read => AQUA,
        ActivityKind::Search => Color::LightBlue,
        ActivityKind::Write => Color::Yellow,
        ActivityKind::Shell => Color::Magenta,
        ActivityKind::Skill => CRAB,
        ActivityKind::Network => Color::LightRed,
        ActivityKind::Other => MUTED,
    };
    let (icon, icon_color) = match activity.status {
        ActivityStatus::Running => (
            SPINNER[app.spinner % SPINNER.len()].to_string(),
            Color::Yellow,
        ),
        ActivityStatus::Completed => ("✓".to_owned(), Color::Green),
        ActivityStatus::Failed => ("×".to_owned(), Color::Red),
    };
    let icon = format!("{icon} ");
    let title = format!("{} ", activity.title);
    let detail = activity_detail_for_display(&app.project_root, activity);
    let detail_start = "  ".len() + icon.len() + title.len();
    let line_index = source.lines.len();
    source.lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(icon, Style::default().fg(icon_color)),
        Span::styled(
            title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(detail.clone(), Style::default().fg(MUTED)),
    ]));
    if matches!(
        activity.tool.as_str(),
        "shell" | "read_file" | "write_file" | "replace_in_file"
    ) && !detail.is_empty()
    {
        source.copy_targets.push(CopyTarget {
            ranges: vec![SourceRange {
                line: line_index,
                start: detail_start,
                end: detail_start + detail.len(),
            }],
            text: detail,
        });
    }
}

fn wrap_conversation_lines(lines: &[Line<'static>], width: u16) -> Vec<VisualRow> {
    let mut rows = Vec::new();
    for (source_line, line) in lines.iter().enumerate() {
        let mut row = VisualRow {
            source_line,
            units: Vec::new(),
        };
        let mut row_width = 0;
        let mut source_offset = 0;
        for span in &line.spans {
            for character in span.content.chars() {
                let source_start = source_offset;
                source_offset += character.len_utf8();
                let (text, character_width) = if character == '\t' {
                    ("    ".to_owned(), 4)
                } else {
                    (
                        character.to_string(),
                        UnicodeWidthChar::width(character).unwrap_or(0) as u16,
                    )
                };
                if character_width == 0
                    && let Some(previous) = row.units.last_mut()
                {
                    previous.text.push_str(&text);
                    previous.source_end = source_offset;
                    continue;
                }
                if !row.units.is_empty() && row_width + character_width > width {
                    rows.push(std::mem::take(&mut row));
                    row.source_line = source_line;
                    row_width = 0;
                }
                row.units.push(VisualUnit {
                    text,
                    width: character_width,
                    style: span.style,
                    source_line,
                    source_start,
                    source_end: source_offset,
                });
                row_width = row_width.saturating_add(character_width);
            }
        }
        rows.push(row);
    }
    rows
}

fn visual_row_line(app: &App, row_index: usize, row: &VisualRow) -> Line<'static> {
    Line::from(
        row.units
            .iter()
            .enumerate()
            .map(|(unit_index, unit)| {
                let point = TextPoint {
                    row: row_index,
                    unit: unit_index,
                };
                let selected = app
                    .text_selection
                    .as_ref()
                    .is_some_and(|selection| selection.contains(point));
                let flashed = app.copy_flash.as_ref().is_some_and(|flash| {
                    Instant::now() < flash.until
                        && flash.ranges.iter().any(|range| {
                            range.line == unit.source_line
                                && unit.source_start < range.end
                                && unit.source_end > range.start
                        })
                });
                let style = if selected || flashed {
                    unit.style.add_modifier(Modifier::REVERSED)
                } else {
                    unit.style
                };
                Span::styled(unit.text.clone(), style)
            })
            .collect::<Vec<_>>(),
    )
}

fn activity_detail_for_display(project_root: &Path, activity: &AgentActivity) -> String {
    if !matches!(
        activity.tool.as_str(),
        "list_files" | "read_file" | "write_file" | "replace_in_file"
    ) {
        return activity.detail.clone();
    }
    let path = Path::new(&activity.detail);
    if !path.is_absolute() {
        return activity.detail.clone();
    }
    let path = normalized_root(path);
    let root = normalized_root(project_root);
    path.strip_prefix(&root)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| activity.detail.clone())
}

fn render_composer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let color = if app.recording.is_some() {
        Color::Red
    } else if app.is_running() || app.transcription.is_some() {
        Color::Yellow
    } else {
        AQUA
    };
    let recording_title;
    let title = if app.recording.is_some() {
        recording_title = format!(
            " Recording · {} to stop · Enter to send ",
            dictation_shortcut_label()
        );
        recording_title.as_str()
    } else if app.transcription.is_some() {
        " Transcribing voice… "
    } else if app.is_running() {
        " Queue next message "
    } else if app.editing_goal_id.is_some() {
        " Edit goal · Enter save · Esc cancel "
    } else {
        " Message "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Span::styled(title, Style::default().fg(color)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some(recording) = &app.recording {
        let waveform = inner.inner(Margin {
            horizontal: 2,
            vertical: 0,
        });
        let waveform = Rect::new(
            waveform.x,
            waveform.y + waveform.height.saturating_sub(1) / 2,
            waveform.width,
            waveform.height.min(1),
        );
        frame.render_widget(
            Paragraph::new(waveform_line(recording.waveform(waveform.width as usize))),
            waveform,
        );
        return;
    }

    let rows = composer_rows(&app.input, inner.width.max(1) as usize);
    let (line_index, column) = composer_cursor_position(&app.input, &rows, app.cursor);
    let visible = inner.height.max(1) as usize;
    let start = line_index.saturating_sub(visible - 1);
    let shown = rows
        .iter()
        .skip(start)
        .take(visible)
        .map(|row| Line::from(app.input[row.start..row.end].to_owned()))
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
    if !app.show_help
        && !app.show_skills
        && app.model_picker.is_none()
        && app.session_picker.is_none()
        && app.goal_picker.is_none()
    {
        frame.set_cursor_position((
            inner.x + column.min(inner.width.saturating_sub(1) as usize) as u16,
            inner.y + line_index.saturating_sub(start) as u16,
        ));
    }
}

fn waveform_line(levels: impl IntoIterator<Item = u8>) -> Line<'static> {
    Line::from(
        levels
            .into_iter()
            .map(|level| {
                Span::styled(
                    WAVEFORM[usize::from(level.min((WAVEFORM.len() - 1) as u8))].to_string(),
                    Style::default().fg(if level == 0 { Color::DarkGray } else { CRAB }),
                )
            })
            .collect::<Vec<_>>(),
    )
}

fn render_completion(frame: &mut Frame<'_>, app: &App, body: Rect, composer: Rect) {
    let Some(menu) = &app.completion else {
        return;
    };
    let is_file_menu = menu
        .items
        .iter()
        .any(|item| matches!(item.kind, CompletionKind::File | CompletionKind::Directory));
    let desired_rows = menu.items.len().min(14) as u16;
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
                        item.display.clone(),
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

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(area, 74, 24);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            "Keyboard",
            Style::default().fg(CRAB).add_modifier(Modifier::BOLD),
        )),
        Line::from("  Enter                 complete, send, or send recording"),
        Line::from("  Tab                   complete slash selection"),
        Line::from(format!("  {:<30}insert newline", newline_shortcut_label())),
        Line::from(format!(
            "  {:<30}start or stop voice dictation",
            dictation_shortcut_label()
        )),
        Line::from("  ↑ / ↓                 navigate menu or move between lines"),
        Line::from("  PgUp / PgDn           scroll conversation"),
        Line::from("  Ctrl+U / Ctrl+K       delete to line start / end"),
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
        Line::from("  /branches  browse conversation branches"),
        Line::from("  /goal ...  start a persistent goal"),
        Line::from("  /goals     manage persistent goals"),
        Line::from("  /model     choose model, thinking, and speed"),
        Line::from("  /sessions  resume or delete saved sessions"),
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
                .map(|option| (option.name.clone(), option.description.clone()))
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

fn render_session_picker(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(picker) = &app.session_picker else {
        return;
    };
    let rows = picker.rows();
    let session_count = picker
        .projects
        .iter()
        .map(|project| project.project.sessions.len())
        .sum::<usize>();
    let height = (rows.len().min(16) as u16 + 5).clamp(8, area.height.saturating_sub(2).max(8));
    let popup = centered_rect(area, 86, height);
    frame.render_widget(Clear, popup);
    let available = popup.height.saturating_sub(4) as usize;
    let start = picker
        .selected
        .saturating_sub(available.saturating_sub(1))
        .min(rows.len().saturating_sub(available));
    let current_id = Some(app.conversation.snapshot().session.id);
    let mut lines = vec![
        Line::from(Span::styled(
            "↑↓ select  •  ← parent/collapse  •  → expand  •  Enter resume  •  Del delete",
            Style::default().fg(MUTED),
        )),
        Line::default(),
    ];
    for (index, row) in rows.iter().enumerate().skip(start).take(available) {
        let selected = index == picker.selected;
        let line = match *row {
            SessionPickerRow::Project(project_index) => {
                let project = &picker.projects[project_index];
                let active = paths_equal(&project.project.root, &app.project_root);
                Line::from(vec![
                    Span::styled(
                        if selected { " › " } else { "   " },
                        Style::default().fg(CRAB),
                    ),
                    Span::styled(
                        if project.expanded { "▾ " } else { "▸ " },
                        Style::default().fg(CRAB),
                    ),
                    Span::styled(
                        compact_path(&project.project.root.display().to_string(), 54),
                        Style::default()
                            .fg(if selected { Color::White } else { CRAB })
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            "  {}{}",
                            project.project.sessions.len(),
                            if active { "  current project" } else { "" }
                        ),
                        Style::default().fg(MUTED),
                    ),
                ])
            }
            SessionPickerRow::Session(project_index, session_index) => {
                let session = &picker.projects[project_index].project.sessions[session_index];
                let active = Some(session.id) == current_id
                    && paths_equal(
                        &picker.projects[project_index].project.root,
                        &app.project_root,
                    );
                let lifecycle = app
                    .conversations
                    .statuses()
                    .into_iter()
                    .find(|status| status.id == session.id)
                    .map(|status| status.lifecycle);
                let status = match lifecycle {
                    Some(ConversationLifecycle::Running) => "running",
                    Some(ConversationLifecycle::Failed) => "error",
                    Some(ConversationLifecycle::Stopping) => "stopping",
                    Some(ConversationLifecycle::Idle) => "idle",
                    _ => "",
                };
                Line::from(vec![
                    Span::styled(
                        if selected { " ›     " } else { "       " },
                        Style::default().fg(AQUA),
                    ),
                    Span::styled(if active { "● " } else { "  " }, Style::default().fg(AQUA)),
                    Span::styled(
                        format!("{:<28}", compact_text(&session.title, 27)),
                        Style::default()
                            .fg(if selected { Color::White } else { AQUA })
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            " {:<18}  {:<8}  {}  {}",
                            compact_text(&session.model, 17),
                            status,
                            &session.id.to_string()[..8],
                            session.updated_at.format("%Y-%m-%d %H:%M")
                        ),
                        Style::default().fg(MUTED),
                    ),
                ])
            }
        };
        lines.push(line.style(if selected {
            Style::default().bg(Color::Rgb(42, 48, 58))
        } else {
            Style::default()
        }));
    }
    if rows.is_empty() {
        lines.push(Line::from("No saved sessions."));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(CRAB))
                .title(Span::styled(
                    format!(
                        " Sessions · {} projects · {} sessions ",
                        picker.projects.len(),
                        session_count
                    ),
                    Style::default().fg(CRAB).add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );
}

fn render_goal_picker(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(picker) = &app.goal_picker else {
        return;
    };
    let height =
        (app.goals.len().min(16) as u16 + 5).clamp(8, area.height.saturating_sub(2).max(8));
    let popup = centered_rect(area, 82, height);
    frame.render_widget(Clear, popup);
    let available = popup.height.saturating_sub(4) as usize;
    let start = picker
        .selected
        .saturating_sub(available.saturating_sub(1))
        .min(app.goals.len().saturating_sub(available));
    let mut lines = vec![
        Line::from(Span::styled(
            "↑↓ select  •  Enter/Space play or pause  •  D describe  •  Del delete  •  Esc close",
            Style::default().fg(MUTED),
        )),
        Line::default(),
    ];
    for (index, goal) in app.goals.iter().enumerate().skip(start).take(available) {
        let selected = index == picker.selected;
        let (status, color) = match goal.status {
            GoalStatus::Active => ("ACTIVE", GOAL),
            GoalStatus::Paused => ("PAUSED", Color::Yellow),
            GoalStatus::Completed => ("DONE", AQUA),
            GoalStatus::Blocked => ("BLOCKED", Color::Red),
        };
        let preview = goal.objective.replace('\n', " ");
        lines.push(
            Line::from(vec![
                Span::styled(
                    if selected { " › " } else { "   " },
                    Style::default().fg(GOAL),
                ),
                Span::styled(
                    format!("{status:<8}"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    compact_text(&preview, 62),
                    Style::default().fg(if selected { Color::White } else { MUTED }),
                ),
            ])
            .style(if selected {
                Style::default().bg(Color::Rgb(42, 48, 58))
            } else {
                Style::default()
            }),
        );
    }
    if app.goals.is_empty() {
        lines.push(Line::from("No goals in this session."));
        lines.push(Line::from(Span::styled(
            "Create one with /goal followed by its objective.",
            Style::default().fg(MUTED),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(GOAL))
                .title(Span::styled(
                    format!(" Goals ({}) ", app.goals.len()),
                    Style::default().fg(GOAL).add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );

    if picker.describing
        && let Some(goal) = app.goals.get(picker.selected)
    {
        let description = centered_rect(area, 72, area.height.saturating_sub(8).clamp(8, 24));
        frame.render_widget(Clear, description);
        frame.render_widget(
            Paragraph::new(goal.objective.clone())
                .wrap(Wrap { trim: false })
                .scroll((picker.description_scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(GOAL))
                        .title(Span::styled(
                            " Goal description · ↑↓/PgUp/PgDn scroll · Esc close ",
                            Style::default().fg(GOAL).add_modifier(Modifier::BOLD),
                        )),
                ),
            description,
        );
    }
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

fn composer_rows(input: &str, width: usize) -> Vec<ComposerRow> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut start = 0;
    let mut used = 0_usize;
    for (index, character) in input.char_indices() {
        if character == '\n' {
            rows.push(ComposerRow { start, end: index });
            start = index + character.len_utf8();
            used = 0;
            continue;
        }
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used > 0 && used.saturating_add(character_width) > width {
            rows.push(ComposerRow { start, end: index });
            start = index;
            used = 0;
        }
        used = used.saturating_add(character_width);
    }
    rows.push(ComposerRow {
        start,
        end: input.len(),
    });
    if used >= width && start < input.len() {
        rows.push(ComposerRow {
            start: input.len(),
            end: input.len(),
        });
    }
    rows
}

fn composer_cursor_position(input: &str, rows: &[ComposerRow], cursor: usize) -> (usize, usize) {
    let row = rows
        .partition_point(|row| row.start <= cursor)
        .saturating_sub(1)
        .min(rows.len().saturating_sub(1));
    let range = rows[row];
    let cursor = cursor.clamp(range.start, range.end);
    (row, display_width(&input[range.start..cursor]))
}

fn byte_index_at_display_column(input: &str, row: ComposerRow, column: usize) -> usize {
    let mut used = 0_usize;
    for (offset, character) in input[row.start..row.end].char_indices() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used.saturating_add(character_width) > column {
            return row.start + offset;
        }
        used = used.saturating_add(character_width);
    }
    row.end
}

fn display_width(text: &str) -> usize {
    text.chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum()
}

fn is_dictation_shortcut(key: &KeyEvent) -> bool {
    is_dictation_shortcut_for_platform(key, cfg!(target_os = "macos"))
}

fn is_dictation_shortcut_for_platform(key: &KeyEvent, macos: bool) -> bool {
    if !matches!(key.code, KeyCode::Char('s' | 'S'))
        || key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return false;
    }
    key.modifiers.contains(KeyModifiers::CONTROL)
        && (macos || key.modifiers.contains(KeyModifiers::SHIFT))
}

fn newline_shortcut_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Alt+Enter / Ctrl+J"
    } else {
        "Shift+Enter / Alt+Enter / Ctrl+J"
    }
}

fn dictation_shortcut_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Ctrl+S"
    } else {
        "Ctrl+Shift+S"
    }
}

fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn editor_action(key: &KeyEvent) -> Option<EditorAction> {
    let word_modifier = key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    let control_sequence =
        key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT);
    let alt_sequence =
        key.modifiers.contains(KeyModifiers::ALT) && !key.modifiers.contains(KeyModifiers::CONTROL);
    let line_modifier = key
        .modifiers
        .intersects(KeyModifiers::SUPER | KeyModifiers::META);
    match key.code {
        KeyCode::Left if word_modifier => Some(EditorAction::MoveWordLeft),
        KeyCode::Right if word_modifier => Some(EditorAction::MoveWordRight),
        KeyCode::Backspace if word_modifier => Some(EditorAction::DeleteWordLeft),
        KeyCode::Delete if word_modifier => Some(EditorAction::DeleteWordRight),
        KeyCode::Char('b' | 'B') if alt_sequence => Some(EditorAction::MoveWordLeft),
        KeyCode::Char('f' | 'F') if alt_sequence => Some(EditorAction::MoveWordRight),
        KeyCode::Char('d' | 'D') if alt_sequence => Some(EditorAction::DeleteWordRight),
        KeyCode::Char('w' | 'W') if control_sequence => Some(EditorAction::DeleteWordLeft),
        KeyCode::Home | KeyCode::Char('a' | 'A')
            if matches!(key.code, KeyCode::Home) || control_sequence =>
        {
            Some(EditorAction::MoveLineStart)
        }
        KeyCode::End | KeyCode::Char('e' | 'E')
            if matches!(key.code, KeyCode::End) || control_sequence =>
        {
            Some(EditorAction::MoveLineEnd)
        }
        KeyCode::Left if line_modifier => Some(EditorAction::MoveLineStart),
        KeyCode::Right if line_modifier => Some(EditorAction::MoveLineEnd),
        KeyCode::Backspace if line_modifier => Some(EditorAction::DeleteToLineStart),
        KeyCode::Delete if line_modifier => Some(EditorAction::DeleteToLineEnd),
        KeyCode::Char('u' | 'U') if control_sequence => Some(EditorAction::DeleteToLineStart),
        KeyCode::Char('k' | 'K') if control_sequence => Some(EditorAction::DeleteToLineEnd),
        _ => None,
    }
}

fn word_left_index(input: &str, cursor: usize) -> usize {
    let mut position = cursor;
    while let Some((index, character)) = previous_character(input, position) {
        if is_word_character(character) {
            break;
        }
        position = index;
    }
    while let Some((index, character)) = previous_character(input, position) {
        if !is_word_character(character) {
            break;
        }
        position = index;
    }
    position
}

fn word_right_index(input: &str, cursor: usize) -> usize {
    let mut position = cursor;
    let starts_in_word = next_character(input, position).is_some_and(is_word_character);
    if starts_in_word {
        while let Some(character) = next_character(input, position) {
            if !is_word_character(character) {
                break;
            }
            position += character.len_utf8();
        }
    }
    while let Some(character) = next_character(input, position) {
        if is_word_character(character) {
            break;
        }
        position += character.len_utf8();
    }
    position
}

fn previous_character(input: &str, position: usize) -> Option<(usize, char)> {
    input[..position].char_indices().next_back()
}

fn next_character(input: &str, position: usize) -> Option<char> {
    input[position..].chars().next()
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn hard_line_start(input: &str, cursor: usize) -> usize {
    input[..cursor]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0)
}

fn hard_line_end(input: &str, cursor: usize) -> usize {
    input[cursor..]
        .find('\n')
        .map(|offset| cursor + offset)
        .unwrap_or(input.len())
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

fn compact_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let head = text.chars().take(max.saturating_sub(1)).collect::<String>();
    format!("{head}…")
}

pub(crate) fn print_sessions(projects: &[SessionProject]) {
    if projects.iter().all(|project| project.sessions.is_empty()) {
        println!("No saved sessions.");
        return;
    }
    for project in projects {
        if project.sessions.is_empty() {
            continue;
        }
        println!("\n{}", project.root.display());
        println!("  {:<10}  {:<20}  {:<18}  TITLE", "ID", "UPDATED", "MODEL");
        for session in &project.sessions {
            println!(
                "  {:<10}  {:<20}  {:<18}  {}",
                &session.id.to_string()[..8],
                session.updated_at.format("%Y-%m-%d %H:%M"),
                session.model,
                session.title
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;
    use ratatui::backend::TestBackend;

    use crate::{
        completion::{NERD_FOLDER, file_completion_context, file_icon},
        config::{Config, SessionRegistry, paths_equal},
        provider::{
            FunctionCall, ModelCatalogEntry, OpenAiCompatible, ReasoningOption, ServiceTierOption,
            ToolCall,
        },
        skills::SkillRegistry,
        tools::ToolBox,
    };

    fn test_registry(root: &Path) -> SessionRegistry {
        SessionRegistry::at(root.join("test-global-config.toml"))
    }

    fn test_app(root: &std::path::Path) -> App {
        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let store = SessionStore::new(root).unwrap();
        let session = store
            .create(
                config
                    .provider(&config.active_provider)
                    .unwrap()
                    .model
                    .clone(),
            )
            .unwrap();
        test_app_with_session(root, config, session)
    }

    fn test_app_with_session(root: &std::path::Path, config: Config, session: Session) -> App {
        let provider = OpenAiCompatible::new(&config, &config.active_provider).unwrap();
        let tools = ToolBox::new(root.to_path_buf());
        App::new(
            Agent::new(provider, tools, SkillRegistry::default(), session).unwrap(),
            Vec::new(),
            None,
            false,
            config,
            test_registry(root),
        )
        .unwrap()
    }

    fn branching_test_app(root: &Path) -> (App, Uuid, Uuid) {
        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let store = SessionStore::new(root).unwrap();
        let mut session = store
            .create(
                config
                    .provider(&config.active_provider)
                    .unwrap()
                    .model
                    .clone(),
            )
            .unwrap();
        let root_node = session
            .messages
            .push(Message::text(Role::User, "root request"));
        session
            .messages
            .push(Message::text(Role::Assistant, "original answer"));
        let original_leaf = session
            .messages
            .push(Message::text(Role::User, "original follow-up"));
        session
            .messages
            .branch_from(
                Some(root_node),
                Message::text(Role::Assistant, "newer answer"),
            )
            .unwrap();
        let newer_leaf = session
            .messages
            .push(Message::text(Role::User, "newer follow-up"));
        (
            test_app_with_session(root, config, session),
            original_leaf,
            newer_leaf,
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
    fn branch_navigator_previews_nodes_and_escape_restores_the_original_branch() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, original_leaf, newer_leaf) = branching_test_app(root.path());
        assert!(app.transcript_node_ids.contains(&newer_leaf));

        app.open_branch_navigator();
        app.move_branch_selection(KeyCode::Up).unwrap();

        assert!(app.transcript_node_ids.contains(&original_leaf));
        assert!(!app.transcript_node_ids.contains(&newer_leaf));
        assert_eq!(app.pending_branch_node, Some(original_leaf));

        app.cancel_branch_navigator();

        assert!(app.branch_navigator.is_none());
        assert!(app.transcript_node_ids.contains(&newer_leaf));
        assert!(!app.transcript_node_ids.contains(&original_leaf));
    }

    #[tokio::test]
    async fn branch_navigator_enter_persists_the_previewed_branch() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, original_leaf, newer_leaf) = branching_test_app(root.path());
        app.open_branch_navigator();
        app.move_branch_selection(KeyCode::Up).unwrap();

        app.confirm_branch_selection().await.unwrap();

        assert!(app.branch_navigator.is_none());
        assert!(app.transcript_node_ids.contains(&original_leaf));
        assert!(!app.transcript_node_ids.contains(&newer_leaf));
        assert!(
            app.conversation
                .snapshot()
                .session
                .messages
                .active_node_ids()
                .contains(&original_leaf)
        );
    }

    #[test]
    fn branch_navigator_renders_beside_the_transcript_at_wide_and_compact_widths() {
        for width in [100, 36] {
            let root = tempfile::tempdir().unwrap();
            let (mut app, _, _) = branching_test_app(root.path());
            app.open_branch_navigator();
            let backend = TestBackend::new(width, 24);
            let mut terminal = Terminal::new(backend).unwrap();

            terminal.draw(|frame| render(frame, &mut app)).unwrap();

            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("branches"), "missing branch panel at {width}");
            assert!(text.contains('◉'), "missing selected node at {width}");
            assert!(
                text.contains("newer follow-up"),
                "missing transcript beside branch panel at {width}"
            );
        }
    }

    #[test]
    fn composer_edits_unicode_at_character_boundaries() {
        let input = "hola\n🦀";
        let rows = composer_rows(input, 20);
        let (line, column) = composer_cursor_position(input, &rows, input.len());
        assert_eq!((line, column), (1, 2));
    }

    #[test]
    fn voice_transcript_is_inserted_at_the_cursor_without_sending() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.input = "Reviewthis".into();
        app.cursor = "Review".len();

        app.insert_transcript("the project");

        assert_eq!(app.input, "Review the project this");
        assert_eq!(app.cursor, "Review the project ".len());
        assert!(app.pending_user.is_none());
    }

    #[test]
    fn terminal_paste_normalizes_line_endings_and_never_submits() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.input = "beforeafter".into();
        app.cursor = "before".len();

        assert!(app.handle_paste("one\r\ntwo\rthree\n"));

        assert_eq!(app.input, "beforeone\ntwo\nthree\nafter");
        assert_eq!(app.cursor, "beforeone\ntwo\nthree\n".len());
        assert!(app.pending_user.is_none());
        assert!(app.queued_prompt.is_none());
        assert!(!app.is_running());
    }

    #[test]
    fn terminal_paste_is_ignored_while_a_modal_owns_input() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.input = "draft".into();
        app.cursor = app.input.len();
        app.show_help = true;

        assert!(!app.handle_paste("unexpected\npaste"));
        assert_eq!(app.input, "draft");
    }

    #[tokio::test]
    async fn voice_transcript_can_be_inserted_and_submitted_immediately() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.send_after_transcription = true;
        app.transcription = Some(tokio::spawn(async {
            Ok::<_, anyhow::Error>("spoken prompt".to_owned())
        }));
        for _ in 0..10 {
            if app
                .transcription
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
            {
                break;
            }
            tokio::task::yield_now().await;
        }

        app.finish_transcription_if_ready().await.unwrap();

        assert_eq!(app.pending_user.as_deref(), Some("spoken prompt"));
        assert!(app.input.is_empty());
        assert!(app.running.is_some());
        app.running.take().unwrap().abort();
    }

    #[test]
    fn terminal_waveform_starts_at_the_bottom_and_grows_with_volume() {
        let line = waveform_line([0, 3, 7]);

        assert_eq!(line.to_string(), "▁▄█");
        assert_eq!(line.spans[0].style.fg, Some(Color::DarkGray));
        assert_eq!(line.spans[1].style.fg, Some(CRAB));
    }

    #[test]
    fn streamed_text_deltas_update_one_live_assistant_message() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        app.event_rx = Some(event_rx);
        event_tx
            .send(AgentEvent::AssistantTextDelta {
                delta: "A long ".into(),
                start: true,
                sequence: 5,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::AssistantTextDelta {
                delta: "answer".into(),
                start: false,
                sequence: 5,
                created_at: chrono::Utc::now(),
            })
            .unwrap();

        app.drain_agent_events();

        assert_eq!(app.live_messages.len(), 1);
        assert_eq!(
            app.live_messages[0].content.as_deref(),
            Some("A long answer")
        );
        assert!(app.auto_scroll);
    }

    #[test]
    fn retry_reset_removes_the_partial_streamed_message() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        app.event_rx = Some(event_rx);
        event_tx
            .send(AgentEvent::AssistantTextDelta {
                delta: "incomplete".into(),
                start: true,
                sequence: 8,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        event_tx.send(AgentEvent::AssistantStreamReset).unwrap();

        app.drain_agent_events();

        assert!(app.live_messages.is_empty());
    }

    #[tokio::test]
    async fn submitting_while_the_agent_works_queues_the_next_message() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let (_finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
        app.running = Some(tokio::spawn(async move {
            let _ = finish_rx.await;
            anyhow::bail!("test turn remains pending")
        }));
        app.input = "Follow up after this turn".into();
        app.cursor = app.input.len();

        app.submit().await.unwrap();

        assert_eq!(
            app.queued_prompt.as_deref(),
            Some("Follow up after this turn")
        );
        assert!(app.input.is_empty());
        assert!(app.is_running());
        app.running.take().unwrap().abort();
    }

    #[tokio::test]
    async fn double_escape_and_the_mouse_only_steer_button_cancel_a_turn() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let (_finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
        app.running = Some(tokio::spawn(async move {
            let _ = finish_rx.await;
            anyhow::bail!("test turn remains pending")
        }));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.last_escape.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.last_escape.is_none());

        app.queued_prompt = Some("Steer now".into());
        app.steer_button = Some(Rect::new(10, 4, 9, 3));
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.queued_prompt.as_deref(), Some("Steer now"));
        app.running.take().unwrap().abort();
    }

    #[test]
    fn queued_message_renders_a_compact_steer_button() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.queued_prompt = Some("Use the other implementation".into());
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

        assert!(text.contains("QUEUED"));
        assert!(text.contains("Use the other implementation"));
        assert!(text.contains("Steer"));
        assert!(app.steer_button.is_some());
    }

    #[tokio::test]
    async fn goal_picker_pauses_the_active_goal_and_the_goal_row_has_mouse_controls() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let snapshot = app
            .conversation
            .create_goal("Finish the migration and keep every test green".into())
            .await
            .unwrap();
        let id = snapshot.session.goals[0].id;
        app.apply_snapshot(snapshot);

        app.open_goal_picker();
        assert_eq!(app.goal_picker.as_ref().unwrap().selected, 0);
        app.toggle_selected_goal();
        app.apply_pending_goal_action().await.unwrap();

        assert_eq!(
            app.goals.iter().find(|goal| goal.id == id).unwrap().status,
            GoalStatus::Paused
        );
        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("PAUSED"));
        assert!(text.contains("Finish the migration"));
        assert!(app.goal_buttons.toggle.is_some());
        assert!(app.goal_buttons.edit.is_some());
        assert!(app.goal_buttons.delete.is_some());
        assert!(app.goal_buttons.list.is_some());
    }

    #[test]
    fn hidden_goal_continuations_do_not_render_as_user_messages() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.transcript
            .push(Message::text(Role::User, "Start the goal"));
        app.transcript
            .push(Message::text(Role::Assistant, "First pass."));
        app.transcript.push(Message::hidden_text(
            Role::User,
            "Continue working toward the active goal.",
        ));
        app.transcript
            .push(Message::text(Role::Assistant, "Second pass."));

        let lines = conversation_lines(&app)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            lines
                .iter()
                .filter(|line| line.as_str() == " USER ")
                .count(),
            1
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.as_str() == " CRAB ")
                .count(),
            2
        );
        assert!(!lines.iter().any(|line| line.contains("Continue working")));
        assert!(lines.iter().any(|line| line == "Second pass."));
    }

    #[test]
    fn conversation_copy_targets_contain_only_messages_commands_and_file_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.transcript
            .push(Message::text(Role::User, "Run the checks"));
        app.transcript.push(Message {
            role: Role::Assistant,
            sequence: None,
            created_at: None,
            content: Some("I’ll run the checks and update the file.".into()),
            tool_calls: Some(vec![
                ToolCall {
                    id: "shell-1".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "shell".into(),
                        arguments: r#"{"command":"cargo test --all"}"#.into(),
                    },
                },
                ToolCall {
                    id: "write-1".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "write_file".into(),
                        arguments: r#"{"path":"src/lib.rs","content":"done"}"#.into(),
                    },
                },
            ]),
            tool_call_id: None,
            hidden: false,
        });
        app.activities.extend([
            AgentActivity {
                id: "shell-1".into(),
                turn_message_id: Uuid::nil(),
                turn_message_index: 0,
                sequence: None,
                started_at: None,
                completed_at: None,
                tool: "shell".into(),
                kind: ActivityKind::Shell,
                status: ActivityStatus::Completed,
                title: "Ran".into(),
                detail: "cargo test --all".into(),
            },
            AgentActivity {
                id: "write-1".into(),
                turn_message_id: Uuid::nil(),
                turn_message_index: 0,
                sequence: None,
                started_at: None,
                completed_at: None,
                tool: "write_file".into(),
                kind: ActivityKind::Write,
                status: ActivityStatus::Completed,
                title: "Wrote".into(),
                detail: "src/lib.rs".into(),
            },
        ]);

        let source = conversation_source(&app);
        let copied = source
            .copy_targets
            .iter()
            .map(|target| target.text.as_str())
            .collect::<Vec<_>>();

        assert!(copied.contains(&"Run the checks"));
        assert!(copied.contains(&"I’ll run the checks and update the file."));
        assert!(copied.contains(&"cargo test --all"));
        assert!(copied.contains(&"src/lib.rs"));
        assert!(!copied.iter().any(|text| text.contains("Ran cargo")));
        assert!(!copied.iter().any(|text| text.contains("Wrote src")));
    }

    fn add_terminal_turn(app: &mut App, completed: bool) -> TurnKey {
        let started_at = chrono::Utc::now() - chrono::Duration::seconds(7);
        app.transcript
            .push(Message::text(Role::User, "Run the checks"));
        let mut progress = Message::text(Role::Assistant, "I’ll inspect the project.");
        progress.sequence = Some(1);
        app.transcript.push(progress);
        let mut final_message = Message::text(Role::Assistant, "Everything passes.");
        final_message.sequence = Some(3);
        app.transcript.push(final_message);
        app.activities.push(AgentActivity {
            id: "shell-1".into(),
            turn_message_id: Uuid::nil(),
            turn_message_index: 0,
            sequence: Some(2),
            started_at: Some(started_at),
            completed_at: completed.then_some(started_at + chrono::Duration::seconds(5)),
            tool: "shell".into(),
            kind: ActivityKind::Shell,
            status: if completed {
                ActivityStatus::Completed
            } else {
                ActivityStatus::Running
            },
            title: "Ran".into(),
            detail: "cargo test".into(),
        });
        app.turns.push(AgentTurn {
            message_id: Uuid::nil(),
            message_index: 0,
            started_at,
            completed_at: completed.then_some(started_at + chrono::Duration::seconds(7)),
        });
        TurnKey {
            session_id: app.session_id,
            message_index: 0,
        }
    }

    #[test]
    fn completed_terminal_turns_collapse_progress_above_the_final_message() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let key = add_terminal_turn(&mut app, true);

        let source = conversation_source(&app);
        let lines = source.lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let summary = lines
            .iter()
            .position(|line| line.contains("Worked for 7s · 1 operation"))
            .unwrap();
        let final_message = lines
            .iter()
            .position(|line| line == "Everything passes.")
            .unwrap();

        assert!(!lines.iter().any(|line| line == "I’ll inspect the project."));
        assert!(!lines.iter().any(|line| line.contains("cargo test")));
        assert_eq!(summary + 1, final_message);
        assert_eq!(source.turn_toggles.len(), 1);
        assert_eq!(source.turn_toggles[0].key, key);
    }

    #[test]
    fn active_terminal_turns_stay_expanded_and_completed_summaries_are_mouse_only() {
        let root = tempfile::tempdir().unwrap();
        let mut active = test_app(root.path());
        add_terminal_turn(&mut active, false);
        let active_source = conversation_source(&active);
        let active_lines = active_source
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        assert!(
            active_lines
                .iter()
                .any(|line| line == "I’ll inspect the project.")
        );
        assert!(active_lines.iter().any(|line| line.contains("cargo test")));
        assert!(active_source.turn_toggles.is_empty());

        let mut completed = test_app(root.path());
        let key = add_terminal_turn(&mut completed, true);
        let source = conversation_source(&completed);
        let rows = wrap_conversation_lines(&source.lines, 80);
        let summary_line = source.turn_toggles[0].line;
        let summary_row = rows
            .iter()
            .position(|row| row.source_line == summary_line)
            .unwrap();
        completed.conversation_view = Some(ConversationView {
            area: Rect::new(0, 0, 80, rows.len() as u16),
            rows,
            scroll: 0,
            copy_targets: source.copy_targets,
            turn_toggles: source.turn_toggles,
        });

        completed.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: summary_row as u16,
            modifiers: KeyModifiers::NONE,
        });

        assert!(completed.expanded_turns.contains(&key));
        let expanded = conversation_source(&completed)
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        let progress = expanded
            .iter()
            .position(|line| line == "I’ll inspect the project.")
            .unwrap();
        let activity = expanded
            .iter()
            .position(|line| line.contains("cargo test"))
            .unwrap();
        let summary = expanded
            .iter()
            .position(|line| line.contains("Worked for 7s · 1 operation"))
            .unwrap();
        let final_message = expanded
            .iter()
            .position(|line| line == "Everything passes.")
            .unwrap();
        assert!(progress < activity);
        assert!(activity < summary);
        assert_eq!(summary + 1, final_message);
    }

    #[test]
    fn dragged_selection_copies_visual_text_without_newlines_at_soft_wraps() {
        let lines = vec![Line::from("abcdef"), Line::from("🦀x")];
        let rows = wrap_conversation_lines(&lines, 3);
        let view = ConversationView {
            area: Rect::new(0, 0, 3, 3),
            rows,
            scroll: 0,
            copy_targets: Vec::new(),
            turn_toggles: Vec::new(),
        };
        let selection = TextSelection {
            anchor: TextPoint { row: 0, unit: 1 },
            cursor: TextPoint { row: 2, unit: 0 },
            dragging: false,
            moved: true,
            last_column: 0,
            last_row: 0,
        };

        assert_eq!(view.selected_text(&selection), "bcdef\n🦀");
    }

    #[test]
    fn copy_feedback_reverses_the_target_style() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.copy_flash = Some(CopyFlash {
            ranges: vec![SourceRange {
                line: 4,
                start: 0,
                end: 4,
            }],
            until: Instant::now() + Duration::from_millis(500),
        });
        let row = VisualRow {
            source_line: 4,
            units: vec![VisualUnit {
                text: "copy".into(),
                width: 4,
                style: Style::default().fg(AQUA),
                source_line: 4,
                source_start: 0,
                source_end: 4,
            }],
        };

        let line = visual_row_line(&app, 0, &row);

        assert!(
            line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn markdown_highlighting_preserves_markers_and_distinguishes_inline_syntax() {
        let highlighter = shared_markdown_highlighter();
        let markdown = concat!(
            "# Main\n",
            "## Secondary\n",
            "### Also secondary\n",
            "**bold** *italic* `inline`\n",
            "- bullet\n",
            "12. numbered"
        );

        let lines = highlighter.render(markdown);
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(rendered, markdown.lines().collect::<Vec<_>>());
        assert_eq!(lines[0].spans[0].style.fg, Some(MARKDOWN_H1));
        assert_eq!(lines[1].spans[0].style.fg, Some(MARKDOWN_HEADING));
        assert_eq!(lines[2].spans[0].style.fg, Some(MARKDOWN_HEADING));
        let bold = lines[3]
            .spans
            .iter()
            .find(|span| span.content == "**bold**")
            .unwrap();
        assert_eq!(bold.style.fg, Some(MARKDOWN_BOLD));
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let italic = lines[3]
            .spans
            .iter()
            .find(|span| span.content == "*italic*")
            .unwrap();
        assert_eq!(italic.style.fg, Some(MARKDOWN_ITALIC));
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
        let inline = lines[3]
            .spans
            .iter()
            .find(|span| span.content == "`inline`")
            .unwrap();
        assert_eq!(inline.style.fg, Some(MARKDOWN_CODE));
        assert_eq!(lines[4].spans[0].content, "-");
        assert_eq!(lines[4].spans[0].style.fg, Some(MARKDOWN_LIST));
        assert_eq!(lines[5].spans[0].content, "12.");
        assert_eq!(lines[5].spans[0].style.fg, Some(MARKDOWN_LIST));
    }

    #[test]
    fn fenced_code_uses_embedded_syntax_highlighting_and_keeps_the_fences() {
        let highlighter = shared_markdown_highlighter();
        let markdown = "```rust\nfn main() { let value = 42; }\n```";

        let lines = highlighter.render(markdown);
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let code_colors = lines[1]
            .spans
            .iter()
            .filter_map(|span| span.style.fg)
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(
            rendered,
            ["```rust", "fn main() { let value = 42; }", "```"]
        );
        assert!(code_colors.len() > 1);
        assert_eq!(lines[0].spans[0].content, "```");
        assert_eq!(lines[0].spans[0].style.fg, Some(MARKDOWN_FENCE));
    }

    #[test]
    fn markdown_styles_apply_to_agent_messages_but_not_user_messages() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.transcript
            .push(Message::text(Role::User, "**user text**"));
        app.transcript
            .push(Message::text(Role::Assistant, "**agent text**"));

        let source = conversation_source(&app);
        let user = source
            .lines
            .iter()
            .find(|line| line.to_string() == "**user text**")
            .unwrap();
        let agent = source
            .lines
            .iter()
            .find(|line| line.to_string() == "**agent text**")
            .unwrap();

        assert!(!user.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(agent.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(agent.spans[0].content, "**agent text**");
    }

    #[test]
    fn dragging_above_the_conversation_scrolls_and_extends_the_selection() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.scroll = 5;
        app.max_scroll = 15;
        app.conversation_view = Some(ConversationView {
            area: Rect::new(0, 3, 40, 5),
            rows: (0..20)
                .map(|source_line| VisualRow {
                    source_line,
                    units: vec![VisualUnit {
                        text: "x".into(),
                        width: 1,
                        style: Style::default(),
                        source_line,
                        source_start: 0,
                        source_end: 1,
                    }],
                })
                .collect(),
            scroll: 5,
            copy_targets: Vec::new(),
            turn_toggles: Vec::new(),
        });
        app.text_selection = Some(TextSelection {
            anchor: TextPoint { row: 7, unit: 0 },
            cursor: TextPoint { row: 7, unit: 0 },
            dragging: true,
            moved: false,
            last_column: 0,
            last_row: 1,
        });

        app.update_drag_autoscroll();

        assert_eq!(app.scroll, 3);
        assert_eq!(
            app.text_selection.as_ref().unwrap().cursor,
            TextPoint { row: 3, unit: 0 }
        );
        assert!(app.text_selection.as_ref().unwrap().moved);
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

    #[tokio::test]
    async fn printable_characters_use_the_terminal_resolved_keyboard_layout() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());

        // Spanish layout: AltGr+2 is reported by Windows as Ctrl+Alt+'@'.
        app.handle_key(KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .await
        .unwrap();
        // US layout: Shift+2 is already reported as the logical '@' character.
        app.handle_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::SHIFT))
            .await
            .unwrap();
        app.handle_key(KeyEvent::new(
            KeyCode::Char('€'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .await
        .unwrap();
        app.handle_key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .await
        .unwrap();

        assert_eq!(app.input, "@@€b");

        // An unknown Ctrl-only chord remains a control chord rather than text.
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(app.input, "@@€b");
    }

    #[test]
    fn renders_wide_and_compact_layouts() {
        let wide = render_text(120, 36);
        assert!(wide.contains("CODECRAB"));
        assert!(!wide.contains("Conversation"));
        assert!(!wide.contains("Activity"));
        assert!(wide.contains("default"));
        assert!(!wide.contains("What are we building?"));
        assert!(!wide.contains("Enter send"));

        let compact = render_text(70, 24);
        assert!(compact.contains("CODECRAB"));
        assert!(!compact.contains("Activity"));
        assert!(compact.contains("Message"));
    }

    #[tokio::test]
    async fn conversation_scroll_stays_manual_during_new_agent_output_until_bottom() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        app.event_rx = Some(event_rx);
        app.max_scroll = 30;
        app.scroll = 30;
        app.auto_scroll = true;
        app.activities.push(AgentActivity {
            id: "old-call".into(),
            turn_message_id: Uuid::nil(),
            turn_message_index: 0,
            sequence: None,
            started_at: None,
            completed_at: None,
            tool: "shell".into(),
            kind: ActivityKind::Shell,
            status: ActivityStatus::Completed,
            title: "Ran".into(),
            detail: "cargo test".into(),
        });

        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.scroll, 20);
        assert!(!app.auto_scroll);

        event_tx
            .send(AgentEvent::AssistantTextDelta {
                delta: "New output".into(),
                start: true,
                sequence: 1,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        app.drain_agent_events();
        assert_eq!(app.live_messages[0].content.as_deref(), Some("New output"));
        assert_eq!(app.scroll, 20);
        assert!(!app.auto_scroll);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.scroll, 17);
        assert!(!app.auto_scroll);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.scroll, 20);
        assert!(!app.auto_scroll);

        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.scroll, 30);
        assert!(app.auto_scroll);
    }

    #[test]
    fn agent_actions_are_grouped_under_one_crab_turn() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.transcript
            .push(Message::text(Role::User, "Inspect the project"));
        app.transcript.push(Message {
            role: Role::Assistant,
            sequence: None,
            created_at: None,
            content: Some("I’ll inspect the relevant file first.".into()),
            tool_calls: Some(vec![ToolCall {
                id: "call-1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{"path":"src/main.rs"}"#.into(),
                },
            }]),
            tool_call_id: None,
            hidden: false,
        });
        app.transcript.push(Message {
            role: Role::Tool,
            sequence: None,
            created_at: None,
            content: Some("file contents".into()),
            tool_calls: None,
            tool_call_id: Some("call-1".into()),
            hidden: false,
        });
        app.activities.push(AgentActivity {
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
        });
        app.transcript
            .push(Message::text(Role::Assistant, "Inspection complete."));

        let lines = conversation_lines(&app)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            lines
                .iter()
                .filter(|line| line.as_str() == " CRAB ")
                .count(),
            1
        );
        let user = lines.iter().position(|line| line == " USER ").unwrap();
        let crab = lines.iter().position(|line| line == " CRAB ").unwrap();
        let action = lines
            .iter()
            .position(|line| line.contains("Read src/main.rs"))
            .unwrap();
        let progress = lines
            .iter()
            .position(|line| line == "I’ll inspect the relevant file first.")
            .unwrap();
        let answer = lines
            .iter()
            .position(|line| line == "Inspection complete.")
            .unwrap();
        assert!(user < crab && crab < progress && progress < action && action < answer);
        assert!(lines[crab - 1].is_empty());
        assert!(lines[action - 1].is_empty());
        assert!(lines[answer - 1].is_empty());
    }

    #[test]
    fn live_messages_and_unmatched_tools_follow_their_chronological_sequence() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.pending_user = Some("Inspect the project".into());
        let mut first = Message::text(Role::Assistant, "First progress");
        first.sequence = Some(0);
        let mut second = Message::text(Role::Assistant, "Second progress");
        second.sequence = Some(3);
        app.live_messages = vec![first, second];
        app.activities.extend([
            AgentActivity {
                id: "call-1".into(),
                turn_message_id: Uuid::nil(),
                turn_message_index: 0,
                sequence: Some(1),
                started_at: None,
                completed_at: None,
                tool: "read_file".into(),
                kind: ActivityKind::Read,
                status: ActivityStatus::Completed,
                title: "Read".into(),
                detail: "src/first.rs".into(),
            },
            AgentActivity {
                id: "call-2".into(),
                turn_message_id: Uuid::nil(),
                turn_message_index: 0,
                sequence: Some(2),
                started_at: None,
                completed_at: None,
                tool: "read_file".into(),
                kind: ActivityKind::Read,
                status: ActivityStatus::Completed,
                title: "Read".into(),
                detail: "src/second.rs".into(),
            },
        ]);

        let lines = conversation_lines(&app)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        let position = |needle: &str| lines.iter().position(|line| line.contains(needle)).unwrap();

        assert!(position("First progress") < position("src/first.rs"));
        assert!(position("src/first.rs") < position("src/second.rs"));
        assert!(position("src/second.rs") < position("Second progress"));
    }

    #[test]
    fn terminal_file_activities_are_relative_to_the_active_project() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("src");
        fs::create_dir(&source).unwrap();
        let file = source.join("main.rs");
        fs::write(&file, "fn main() {}").unwrap();
        let activity = AgentActivity {
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
            detail: file.display().to_string(),
        };

        assert_eq!(
            activity_detail_for_display(root.path(), &activity),
            "src/main.rs"
        );
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
    fn composer_soft_wraps_without_changing_the_prompt() {
        let input = "abcdefghij";
        let rows = composer_rows(input, 4);
        let rendered = rows
            .iter()
            .map(|row| &input[row.start..row.end])
            .collect::<Vec<_>>();

        assert_eq!(rendered, ["abcd", "efgh", "ij"]);
        assert_eq!(input, "abcdefghij");

        let exact = composer_rows("abcd", 4);
        assert_eq!(exact.len(), 2);
        assert_eq!(exact[1], ComposerRow { start: 4, end: 4 });
    }

    #[test]
    fn vertical_arrows_navigate_soft_wrapped_rows() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.input = "abcdefghij".into();
        app.cursor = 2;
        app.composer_width = 4;

        assert!(app.move_vertical(1));
        assert_eq!(app.cursor, 6);
        assert!(app.move_vertical(1));
        assert_eq!(app.cursor, 10);
        assert!(app.move_vertical(-1));
        assert_eq!(app.cursor, 6);
        assert_eq!(app.input, "abcdefghij");
    }

    #[test]
    fn soft_wrap_and_cursor_columns_use_terminal_character_width() {
        let input = "a🦀bc";
        let rows = composer_rows(input, 3);
        let rendered = rows
            .iter()
            .map(|row| &input[row.start..row.end])
            .collect::<Vec<_>>();

        assert_eq!(rendered, ["a🦀", "bc"]);
        assert_eq!(composer_cursor_position(input, &rows, "a🦀".len()), (1, 0));
    }

    #[test]
    fn dictation_shortcut_accounts_for_macos_modifier_collapsing() {
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let ctrl_shift_s = KeyEvent::new(
            KeyCode::Char('S'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let ctrl_alt_s = KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );

        assert!(is_dictation_shortcut_for_platform(&ctrl_s, true));
        assert!(is_dictation_shortcut_for_platform(&ctrl_shift_s, true));
        assert!(!is_dictation_shortcut_for_platform(&ctrl_alt_s, true));
        assert!(!is_dictation_shortcut_for_platform(&ctrl_s, false));
        assert!(is_dictation_shortcut_for_platform(&ctrl_shift_s, false));
    }

    #[test]
    fn decoded_terminal_sequences_drive_word_and_line_actions() {
        assert_eq!(
            editor_action(&KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
            Some(EditorAction::MoveWordLeft)
        );
        assert_eq!(
            editor_action(&KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            Some(EditorAction::DeleteWordLeft)
        );
        assert_eq!(
            editor_action(&KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT)),
            Some(EditorAction::MoveWordRight)
        );
        assert_eq!(
            editor_action(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER)),
            Some(EditorAction::DeleteToLineStart)
        );
        assert_eq!(
            editor_action(&KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            Some(EditorAction::MoveLineStart)
        );
    }

    #[test]
    fn word_and_line_actions_edit_unicode_text_at_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.input = "uno 🦀 dos\núltima línea".into();
        app.cursor = "uno 🦀 dos".len();

        app.apply_editor_action(EditorAction::MoveWordLeft);
        assert_eq!(&app.input[app.cursor..], "dos\núltima línea");
        app.apply_editor_action(EditorAction::DeleteWordLeft);
        assert_eq!(app.input, "dos\núltima línea");

        app.cursor = app.input.len();
        app.apply_editor_action(EditorAction::DeleteToLineStart);
        assert_eq!(app.input, "dos\n");
        assert_eq!(app.cursor, "dos\n".len());
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

    #[tokio::test]
    async fn at_menu_merges_recursive_results_without_moving_the_selection() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(workspace.join("nested/deeper")).unwrap();
        fs::write(workspace.join("local-config.toml"), "").unwrap();
        fs::write(workspace.join("nested/deeper/remote-config.toml"), "").unwrap();
        let mut app = test_app(&workspace);

        app.insert("@config");
        let local_id = app.completion.as_ref().unwrap().items[0].id.clone();
        app.completion.as_mut().unwrap().selected = 0;
        for _ in 0..100 {
            app.drain_completion_updates();
            if app.completion.as_ref().is_some_and(|menu| {
                menu.items
                    .iter()
                    .any(|item| item.name == "nested/deeper/remote-config.toml")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let menu = app.completion.as_ref().unwrap();
        assert!(
            menu.items
                .iter()
                .any(|item| item.display == "nested/deeper/remote-config.toml")
        );
        assert_eq!(menu.items[menu.selected].id, local_id);
    }

    #[tokio::test]
    async fn at_menu_drops_recursive_updates_from_a_stale_query() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(workspace.join("nested")).unwrap();
        fs::write(workspace.join("nested/old-match.toml"), "").unwrap();
        fs::write(workspace.join("nested/new-match.toml"), "").unwrap();
        let mut app = test_app(&workspace);

        app.insert("@old");
        app.input.clear();
        app.cursor = 0;
        app.insert("@new");
        for _ in 0..100 {
            app.drain_completion_updates();
            if app.completion.as_ref().is_some_and(|menu| {
                menu.items
                    .iter()
                    .any(|item| item.name == "nested/new-match.toml")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let menu = app.completion.as_ref().unwrap();
        assert!(
            menu.items
                .iter()
                .any(|item| item.name == "nested/new-match.toml")
        );
        assert!(
            menu.items
                .iter()
                .all(|item| item.name != "nested/old-match.toml")
        );
    }

    #[tokio::test]
    async fn model_picker_walks_model_reasoning_and_service_tier() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.model_catalog = vec![
            ModelCatalogEntry {
                slug: "future-9-sol".into(),
                display_name: "Future-9-Sol".into(),
                default_reasoning_level: Some("low".into()),
                supported_reasoning_levels: vec![
                    ReasoningOption {
                        effort: "low".into(),
                        name: "low".into(),
                        description: "Quick".into(),
                    },
                    ReasoningOption {
                        effort: "deep".into(),
                        name: "deep".into(),
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
        app.accept_model_selection().await.unwrap();
        assert_eq!(
            app.model_picker.as_ref().unwrap().step,
            ModelPickerStep::Reasoning
        );
        app.model_picker.as_mut().unwrap().selected = 1;
        app.accept_model_selection().await.unwrap();
        assert_eq!(
            app.model_picker.as_ref().unwrap().step,
            ModelPickerStep::Speed
        );
        app.model_picker.as_mut().unwrap().selected = 1;
        app.accept_model_selection().await.unwrap();

        assert_eq!(app.model, "future-9-sol");
        assert_eq!(app.reasoning_effort.as_deref(), Some("deep"));
        assert_eq!(app.service_tier.as_deref(), Some("priority"));
        assert!(app.uses_fast_service_tier());
        assert_eq!(
            app.conversation.snapshot().session.service_tier.as_deref(),
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
        let after_model = text.split_once("future-9-sol").unwrap().1;
        let (before_fast, after_fast) = after_model.split_once('⚡').unwrap();
        assert!(!before_fast.contains('│'));
        let before_thinking = after_fast.split_once("deep").unwrap().0;
        assert_eq!(before_thinking.matches('│').count(), 1);
    }

    #[tokio::test]
    async fn session_picker_navigates_projects_and_switches_the_agent_root() {
        let temp = tempfile::tempdir().unwrap();
        let current_root = temp.path().join("current-project");
        let other_root = temp.path().join("other-project");
        fs::create_dir_all(&current_root).unwrap();
        fs::create_dir_all(&other_root).unwrap();
        fs::write(other_root.join("only-in-other.txt"), "hello").unwrap();

        let registry = test_registry(temp.path());
        let other_store = SessionStore::new(&other_root).unwrap();
        let mut app = test_app(&current_root);
        app.registry = registry.clone();
        let mut saved = other_store.create("restored-model".into()).unwrap();
        saved.reasoning_effort = Some("high".into());
        saved.service_tier = Some("priority".into());
        saved.title = "Saved conversation".into();
        saved
            .messages
            .push(Message::text(Role::User, "Remember this"));
        other_store.save(&saved).unwrap();
        registry.register(&other_root).unwrap();

        app.open_session_picker().await.unwrap();
        let picker = app.session_picker.as_ref().unwrap();
        assert!(paths_equal(&picker.projects[0].project.root, &current_root));
        assert!(picker.projects[0].expanded);
        assert!(matches!(
            picker.selected_row(),
            Some(SessionPickerRow::Session(0, _))
        ));

        app.move_session_left();
        assert!(matches!(
            app.session_picker.as_ref().unwrap().selected_row(),
            Some(SessionPickerRow::Project(0))
        ));
        app.move_session_left();
        assert!(!app.session_picker.as_ref().unwrap().projects[0].expanded);
        app.move_session_selection(1);
        assert!(matches!(
            app.session_picker.as_ref().unwrap().selected_row(),
            Some(SessionPickerRow::Project(1))
        ));
        app.move_session_right();
        assert!(app.session_picker.as_ref().unwrap().projects[1].expanded);
        app.move_session_selection(1);
        assert!(matches!(
            app.session_picker.as_ref().unwrap().selected_row(),
            Some(SessionPickerRow::Session(1, _))
        ));

        let backend = TestBackend::new(110, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("other-project"));
        assert!(text.contains("Saved conversation"));
        assert!(text.contains("Del delete"));

        app.accept_session_selection().await.unwrap();
        assert!(paths_equal(&app.project_root, &other_root));
        assert!(paths_equal(
            &app.conversation.snapshot().project_root,
            &other_root
        ));
        assert_eq!(app.model, "restored-model");
        assert_eq!(app.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(app.service_tier.as_deref(), Some("priority"));
        assert_eq!(app.transcript[0].content.as_deref(), Some("Remember this"));

        app.input = "@".into();
        app.cursor = 1;
        app.refresh_completion();
        assert!(
            app.completion
                .as_ref()
                .unwrap()
                .items
                .iter()
                .any(|item| item.name == "only-in-other.txt")
        );

        app.open_session_picker().await.unwrap();
        let saved_row = {
            let picker = app.session_picker.as_ref().unwrap();
            picker
                .rows()
                .iter()
                .position(|row| {
                    matches!(
                        row,
                        SessionPickerRow::Session(project, session)
                            if picker.projects[*project].project.sessions[*session].id == saved.id
                    )
                })
                .unwrap()
        };
        app.session_picker.as_mut().unwrap().selected = saved_row;
        app.delete_session_selection().await.unwrap();

        let replacement = app.conversation.snapshot().session;
        assert_ne!(replacement.id, saved.id);
        assert_eq!(replacement.model, "restored-model");
        assert_eq!(replacement.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(replacement.service_tier.as_deref(), Some("priority"));
        assert!(replacement.messages.is_empty());
        assert!(other_store.load(Some(&saved.id.to_string())).is_err());
        assert!(app.session_picker.is_some());
    }
}
