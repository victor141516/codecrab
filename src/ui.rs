use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
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
    account_usage::{ResetOutcome, ResetResult, UsageState, UsageTracker},
    agent::{Agent, turn_was_cancelled},
    attachments::AttachmentStore,
    audio::AudioRecording,
    completion::{
        CompletionKind, CompletionMenu, CompletionSearch, builtin_command_from_input,
        complete_progressive, goal_objective_from_input, slash_completion_range,
    },
    config::{Config, SessionRegistry, normalized_root, paths_equal},
    conversation::{
        ConversationHandle, ConversationLifecycle, ConversationManager, ConversationSnapshot,
        ConversationTurn,
    },
    coordination::SessionCoordinator,
    cron::{CronDaemonStatus, CronJob, CronStore},
    diagnostics::{DebugOutput, DiagnosticLog},
    events::{ActivityKind, ActivityStatus, AgentActivity, AgentEvent},
    provider::{AttachmentBinding, Message, MessagePart, ModelCatalogEntry, ModelSelection, Role},
    session::{
        AgentTurn, ConversationGraphNode, Goal, GoalStatus, Session, SessionMetadataUpdate,
        SessionProject, SessionScope, SessionStore, SessionSummary, list_session_projects,
    },
    terminal::{TerminalOutputSnapshot, TerminalProcessState, TerminalRecord, TerminalStyle},
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
const USER_MESSAGE_BG: Color = Color::Rgb(13, 31, 49);
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const WAVEFORM: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const TUI_TICK_RATE: Duration = Duration::from_millis(70);
const MAX_TERMINAL_EVENTS_PER_FRAME: usize = 256;
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

struct ProviderPicker {
    providers: Vec<String>,
    selected: usize,
    notice: Option<String>,
}

struct SessionPicker {
    projects: Vec<SessionProjectView>,
    selected: usize,
    collapsed_sessions: HashSet<Uuid>,
}

enum ProcessDialogView {
    List,
    Output(String),
}

struct ProcessDialog {
    processes: Vec<TerminalRecord>,
    selected: usize,
    view: ProcessDialogView,
    output: Option<TerminalOutputSnapshot>,
    output_scroll: u16,
    output_follow: bool,
    stop_confirm: bool,
    last_refresh: Instant,
}

struct GoalPicker {
    selected: usize,
    describing: bool,
    description_scroll: u16,
}

enum UsageTaskResult {
    Refreshed(UsageState),
    Reset {
        result: Result<ResetResult>,
        state: UsageState,
    },
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
    pinned_sessions: Vec<SessionSummary>,
    active_sessions: Vec<SessionSummary>,
    archived_sessions: Vec<SessionSummary>,
    archived_expanded: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionPickerRow {
    Project(usize),
    Section(usize, SessionSection),
    Session(usize, SessionSection, usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionSection {
    Pinned,
    Active,
    Archived,
}

struct SessionRename {
    project_index: usize,
    session_id: Uuid,
    title: String,
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
    user_lines: HashSet<usize>,
    single_row_lines: HashSet<usize>,
    copy_targets: Vec<CopyTarget>,
    turn_toggles: Vec<TurnToggleTarget>,
    node_lines: Vec<(Uuid, usize)>,
    activity_lines: Vec<(String, usize)>,
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
    user_message: bool,
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
    ancestor_continuations: Vec<Vec<Uuid>>,
    following_siblings: Vec<Uuid>,
}

struct MessageEdit {
    node_id: Uuid,
    previous_input: String,
    previous_cursor: usize,
    previous_attachments: Vec<AttachmentBinding>,
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
                if !project.pinned_sessions.is_empty() {
                    rows.push(SessionPickerRow::Section(
                        project_index,
                        SessionSection::Pinned,
                    ));
                    self.append_session_rows(&mut rows, project_index, SessionSection::Pinned);
                }
                self.append_session_rows(&mut rows, project_index, SessionSection::Active);
                if !project.archived_sessions.is_empty() {
                    rows.push(SessionPickerRow::Section(
                        project_index,
                        SessionSection::Archived,
                    ));
                    if project.archived_expanded {
                        self.append_session_rows(
                            &mut rows,
                            project_index,
                            SessionSection::Archived,
                        );
                    }
                }
            }
        }
        rows
    }

    fn append_session_rows(
        &self,
        rows: &mut Vec<SessionPickerRow>,
        project_index: usize,
        section: SessionSection,
    ) {
        let mut hidden_below_depth = None;
        for (session_index, session) in self.projects[project_index]
            .sessions(section)
            .iter()
            .enumerate()
        {
            if let Some(depth) = hidden_below_depth {
                if session.depth > depth {
                    continue;
                }
                hidden_below_depth = None;
            }
            rows.push(SessionPickerRow::Session(
                project_index,
                section,
                session_index,
            ));
            if section != SessionSection::Pinned
                && session.descendant_count > 0
                && self.collapsed_sessions.contains(&session.id)
            {
                hidden_below_depth = Some(session.depth);
            }
        }
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

    fn select_session(&mut self, project_index: usize, session_id: Uuid) {
        if let Some(index) = self.rows().iter().position(|row| {
            let SessionPickerRow::Session(project, section, session) = *row else {
                return false;
            };
            project == project_index
                && section != SessionSection::Pinned
                && self.projects[project].sessions(section)[session].id == session_id
        }) {
            self.selected = index;
        }
    }

    fn selected_session(&self) -> Option<(usize, SessionSection, usize, &SessionSummary)> {
        let SessionPickerRow::Session(project, section, session) = self.selected_row()? else {
            return None;
        };
        Some((
            project,
            section,
            session,
            &self.projects[project].sessions(section)[session],
        ))
    }
}

impl SessionProjectView {
    fn new(project: SessionProject, expanded: bool, archived_expanded: bool) -> Self {
        let pinned_sessions = project.pinned_sessions();
        let active_sessions = project.active_sessions();
        let archived_sessions = project.archived_sessions();
        Self {
            project,
            expanded,
            pinned_sessions,
            active_sessions,
            archived_sessions,
            archived_expanded,
        }
    }

    fn sessions(&self, section: SessionSection) -> &[SessionSummary] {
        match section {
            SessionSection::Pinned => &self.pinned_sessions,
            SessionSection::Active => &self.active_sessions,
            SessionSection::Archived => &self.archived_sessions,
        }
    }
}

#[derive(Clone)]
struct QueuedPrompt {
    id: u64,
    content: String,
    attachments: Vec<AttachmentBinding>,
}

struct PromptQueue {
    items: VecDeque<QueuedPrompt>,
    next_id: u64,
    steered_id: Option<u64>,
}

impl Default for PromptQueue {
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
            next_id: 1,
            steered_id: None,
        }
    }
}

impl PromptQueue {
    fn push(&mut self, content: String, attachments: Vec<AttachmentBinding>) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.items.push_back(QueuedPrompt {
            id,
            content,
            attachments,
        });
        id
    }

    fn update(&mut self, id: u64, content: String, attachments: Vec<AttachmentBinding>) -> bool {
        let Some(prompt) = self.items.iter_mut().find(|prompt| prompt.id == id) else {
            return false;
        };
        prompt.content = content;
        prompt.attachments = attachments;
        true
    }

    fn remove(&mut self, id: u64) -> Option<QueuedPrompt> {
        let index = self.items.iter().position(|prompt| prompt.id == id)?;
        if self.steered_id == Some(id) {
            self.steered_id = None;
        }
        self.items.remove(index)
    }

    fn steer(&mut self, id: u64) -> bool {
        if !self.items.iter().any(|prompt| prompt.id == id) {
            return false;
        }
        self.steered_id = Some(id);
        true
    }

    fn next_id(&self) -> Option<u64> {
        self.steered_id
            .filter(|id| self.items.iter().any(|prompt| prompt.id == *id))
            .or_else(|| self.items.front().map(|prompt| prompt.id))
    }

    fn pop_next(&mut self, editing_id: Option<u64>) -> Option<QueuedPrompt> {
        let id = self.next_id()?;
        if editing_id == Some(id) {
            return None;
        }
        self.steered_id = None;
        self.remove(id)
    }
}

struct QueuedPromptEdit {
    id: u64,
    previous_input: String,
    previous_cursor: usize,
    previous_attachments: Vec<AttachmentBinding>,
}

#[derive(Clone, Copy)]
struct QueuedPromptButtons {
    id: u64,
    steer: Rect,
    edit: Rect,
    delete: Rect,
}

#[derive(Clone, Copy)]
enum QueuedPromptAction {
    Steer(u64),
    Edit(u64),
    Delete(u64),
}

struct App {
    coordinator: SessionCoordinator,
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
    debug_openai: DebugOutput,
    config: Config,
    registry: SessionRegistry,
    clipboard: Option<arboard::Clipboard>,
    markdown: Arc<MarkdownHighlighter>,
    input: String,
    composer_attachments: Vec<AttachmentBinding>,
    cursor: usize,
    preferred_column: Option<usize>,
    composer_width: usize,
    pending_user: Option<String>,
    pending_user_attachments: Vec<AttachmentBinding>,
    prompt_queue: PromptQueue,
    queued_prompt_edit: Option<QueuedPromptEdit>,
    resume_goal_after_queue: bool,
    goals: Vec<Goal>,
    visible_goal_id: Option<Uuid>,
    goal_picker: Option<GoalPicker>,
    pending_goal_action: Option<PendingGoalAction>,
    editing_goal_id: Option<Uuid>,
    editing_goal_resume: bool,
    editing_message: Option<MessageEdit>,
    goal_buttons: GoalButtons,
    last_escape: Option<Instant>,
    queued_prompt_buttons: Vec<QueuedPromptButtons>,
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
    slash_completion_open: bool,
    model_catalog: Vec<ModelCatalogEntry>,
    model_picker: Option<ModelPicker>,
    provider_picker: Option<ProviderPicker>,
    session_picker: Option<SessionPicker>,
    session_rename: Option<SessionRename>,
    process_dialog: Option<ProcessDialog>,
    session_delete_confirm: bool,
    exit_confirm: bool,
    branch_navigator: Option<BranchNavigator>,
    should_quit: bool,
    project: String,
    project_root: PathBuf,
    runtime_root: PathBuf,
    session_scope: SessionScope,
    active_session: bool,
    provider: String,
    model: String,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    usage_tracker: UsageTracker,
    usage_state: UsageState,
    usage_task: Option<JoinHandle<UsageTaskResult>>,
    usage_refresh_pending: bool,
    running_usage_refresh_requested: bool,
    usage_open: bool,
    usage_confirm: bool,
    usage_reset_key: Option<String>,
    usage_scroll: u16,
    usage_notice: Option<String>,
    skills: Vec<SkillView>,
    background_turns: HashMap<Uuid, BackgroundTurn>,
    model_catalogs: HashMap<Uuid, Vec<ModelCatalogEntry>>,
    session_id: Uuid,
    expanded_turns: HashSet<TurnKey>,
    pending_turn_anchor: Option<TurnAnchor>,
    pending_branch_node: Option<Uuid>,
    pending_activity_id: Option<String>,
}

struct BackgroundTurn {
    provider: String,
    running: Option<JoinHandle<Result<ConversationTurn>>>,
    event_rx: Option<mpsc::UnboundedReceiver<AgentEvent>>,
    pending_user: Option<String>,
    pending_user_attachments: Vec<AttachmentBinding>,
    prompt_queue: PromptQueue,
    queued_prompt_edit: Option<QueuedPromptEdit>,
    resume_goal_after_queue: bool,
    pending_goal_action: Option<PendingGoalAction>,
    live_messages: Vec<Message>,
    usage_refresh_requested: bool,
}

struct AppServices {
    debug_openai: DebugOutput,
    usage_tracker: Option<UsageTracker>,
    runtime_root: PathBuf,
}

impl App {
    fn attachment_store(&self) -> Result<AttachmentStore> {
        Ok(SessionStore::for_project_root_in(
            (self.session_scope == SessionScope::Project).then_some(self.project_root.as_path()),
            &self.registry.data_dir()?,
        )?
        .attachment_store())
    }

    fn new(
        agent: Agent,
        model_catalog: Vec<ModelCatalogEntry>,
        catalog_error: Option<String>,
        services: AppServices,
        config: Config,
        registry: SessionRegistry,
        coordinator: SessionCoordinator,
    ) -> Result<Self> {
        let project_root = agent.project_root().to_path_buf();
        let session_scope = agent.session().scope;
        let project = match session_scope {
            SessionScope::Project => project_root.display().to_string(),
            SessionScope::NoProject => "No project".into(),
        };
        let provider = agent.session().provider.clone();
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
        let conversation = coordinator.install(agent)?;
        let session_id = conversation.snapshot().session.id;
        let conversations = coordinator.manager();
        let model_catalogs = HashMap::from([(session_id, model_catalog.clone())]);
        let usage_tracker = services
            .usage_tracker
            .map(Ok)
            .unwrap_or_else(|| UsageTracker::new(services.debug_openai.clone()))?;
        let usage_available = usage_tracker.available_for(&config, &provider);
        let usage_state = if usage_available {
            UsageState::empty()
        } else {
            UsageState::hidden()
        };
        let usage_task = usage_available.then(|| {
            let tracker = usage_tracker.clone();
            let config = config.clone();
            let provider = provider.clone();
            tokio::spawn(async move {
                UsageTaskResult::Refreshed(tracker.refresh_for(&config, &provider).await)
            })
        });
        Ok(Self {
            coordinator,
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
            debug_openai: services.debug_openai,
            config,
            registry,
            clipboard: arboard::Clipboard::new().ok(),
            markdown: shared_markdown_highlighter(),
            input: String::new(),
            composer_attachments: Vec::new(),
            cursor: 0,
            preferred_column: None,
            composer_width: 80,
            pending_user: None,
            pending_user_attachments: Vec::new(),
            prompt_queue: PromptQueue::default(),
            queued_prompt_edit: None,
            resume_goal_after_queue: false,
            goals,
            visible_goal_id,
            goal_picker: None,
            pending_goal_action: None,
            editing_goal_id: None,
            editing_goal_resume: false,
            editing_message: None,
            goal_buttons: GoalButtons::default(),
            last_escape: None,
            queued_prompt_buttons: Vec::new(),
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
            slash_completion_open: false,
            model_catalog,
            model_picker: None,
            provider_picker: None,
            session_picker: None,
            session_rename: None,
            process_dialog: None,
            session_delete_confirm: false,
            exit_confirm: false,
            branch_navigator: None,
            should_quit: false,
            project,
            project_root,
            runtime_root: services.runtime_root,
            session_scope,
            active_session: true,
            provider,
            model,
            reasoning_effort,
            service_tier,
            usage_tracker,
            usage_state,
            usage_task,
            usage_refresh_pending: false,
            running_usage_refresh_requested: false,
            usage_open: false,
            usage_confirm: false,
            usage_reset_key: None,
            usage_scroll: 0,
            usage_notice: None,
            skills,
            background_turns: HashMap::new(),
            model_catalogs,
            session_id,
            expanded_turns: HashSet::new(),
            pending_turn_anchor: None,
            pending_branch_node: None,
            pending_activity_id: None,
        })
    }

    fn is_running(&self) -> bool {
        self.running.is_some()
    }

    fn is_busy(&self) -> bool {
        self.is_running() || self.recording.is_some() || self.transcription.is_some()
    }

    fn status(&self) -> String {
        if !self.active_session {
            "○ no session".into()
        } else if self.recording.is_some() {
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

    fn usage_available(&self) -> bool {
        self.usage_tracker
            .available_for(&self.config, &self.provider)
    }

    fn usage_indicator(&self) -> Option<String> {
        if !self.usage_available() {
            return None;
        }
        let Some(window) = self
            .usage_state
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.windows.first())
        else {
            return Some(if self.usage_task.is_some() {
                "Loading usage…".into()
            } else {
                "Usage unavailable".into()
            });
        };
        let reset = format_local_reset(window.resets_at, "%a %H:%M");
        Some(format!(
            "{}% remaining · Resets {reset}{}",
            window.remaining_percent,
            if self.usage_state.stale {
                " · stale"
            } else {
                ""
            }
        ))
    }

    fn start_usage_refresh(&mut self) {
        if !self.usage_available() {
            return;
        }
        if self.usage_task.is_some() {
            self.usage_refresh_pending = true;
            return;
        }
        let tracker = self.usage_tracker.clone();
        let config = self.config.clone();
        let provider = self.provider.clone();
        self.usage_notice = None;
        self.usage_task = Some(tokio::spawn(async move {
            UsageTaskResult::Refreshed(tracker.refresh_for(&config, &provider).await)
        }));
    }

    fn sync_usage_provider(&mut self) {
        if self.usage_available() {
            if !self.usage_state.available {
                self.usage_state = UsageState::empty();
            }
            self.start_usage_refresh();
        } else {
            self.usage_state = UsageState::hidden();
            self.usage_open = false;
            self.usage_confirm = false;
            self.usage_reset_key = None;
            self.usage_scroll = 0;
            self.usage_refresh_pending = false;
            self.usage_notice = None;
        }
    }

    fn start_usage_reset(&mut self) {
        if self.usage_task.is_some() || !self.usage_state.can_reset {
            return;
        }
        let tracker = self.usage_tracker.clone();
        let config = self.config.clone();
        let provider = self.provider.clone();
        let idempotency_key = self
            .usage_reset_key
            .get_or_insert_with(|| Uuid::new_v4().to_string())
            .clone();
        self.usage_confirm = false;
        self.usage_notice = None;
        self.usage_task = Some(tokio::spawn(async move {
            let result = tracker
                .reset_for(&config, &provider, &idempotency_key, None)
                .await;
            let state = tracker.current_for(&config, &provider).await;
            UsageTaskResult::Reset { result, state }
        }));
    }

    async fn finish_usage_task_if_ready(&mut self) -> Result<()> {
        if !self
            .usage_task
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            return Ok(());
        }
        let task = self.usage_task.take().expect("checked above");
        match task.await.context("usage task failed")? {
            UsageTaskResult::Refreshed(state) => self.usage_state = state,
            UsageTaskResult::Reset { result, state } => {
                self.usage_state = state;
                if result.is_ok() {
                    self.usage_reset_key = None;
                }
                self.usage_notice = Some(match result {
                    Ok(result) => match result.outcome {
                        ResetOutcome::Reset => format!(
                            "Usage reset completed. {} window{} reset.",
                            result.windows_reset,
                            if result.windows_reset == 1 { "" } else { "s" }
                        ),
                        ResetOutcome::NothingToReset => {
                            "No current usage window was eligible for reset.".into()
                        }
                        ResetOutcome::NoCredit => "No reset credits are available.".into(),
                        ResetOutcome::AlreadyRedeemed => {
                            "This reset request was already redeemed.".into()
                        }
                    },
                    Err(error) => format!("Reset failed: {error:#}"),
                });
            }
        }
        if std::mem::take(&mut self.usage_refresh_pending) {
            self.start_usage_refresh();
        }
        Ok(())
    }

    fn insert(&mut self, text: &str) {
        let cursor = self.cursor;
        self.replace_composer_range(cursor, cursor, text);
        self.cursor = cursor + text.len();
        self.preferred_column = None;
        self.refresh_completion();
    }

    fn replace_composer_range(&mut self, start: usize, end: usize, replacement: &str) {
        self.input.replace_range(start..end, replacement);
        let removed = end - start;
        let added = replacement.len();
        self.composer_attachments.retain_mut(|binding| {
            if binding.end <= start {
                true
            } else if binding.start >= end {
                if added >= removed {
                    let delta = added - removed;
                    binding.start += delta;
                    binding.end += delta;
                } else {
                    let delta = removed - added;
                    binding.start -= delta;
                    binding.end -= delta;
                }
                true
            } else {
                false
            }
        });
    }

    fn clear_composer_text(&mut self) {
        self.input.clear();
        self.composer_attachments.clear();
        self.cursor = 0;
    }

    fn trimmed_composer(&self) -> (String, Vec<AttachmentBinding>) {
        let trimmed_start = self.input.len() - self.input.trim_start().len();
        let trimmed_end = self.input.trim_end().len();
        let prompt = self.input[trimmed_start..trimmed_end].to_owned();
        let attachments = self
            .composer_attachments
            .iter()
            .filter(|binding| binding.start >= trimmed_start && binding.end <= trimmed_end)
            .map(|binding| AttachmentBinding {
                attachment_id: binding.attachment_id,
                start: binding.start - trimmed_start,
                end: binding.end - trimmed_start,
            })
            .collect();
        (prompt, attachments)
    }

    fn handle_paste(&mut self, text: &str) -> bool {
        if self.recording.is_some()
            || self.transcription.is_some()
            || self.goal_picker.is_some()
            || self.session_picker.is_some()
            || self.process_dialog.is_some()
            || self.session_delete_confirm
            || self.exit_confirm
            || self.model_picker.is_some()
            || self.provider_picker.is_some()
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

    fn insert_clipboard_item(&mut self, value: &str, attachment_id: Option<Uuid>) {
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
        if leading_space {
            self.insert(" ");
        }
        let start = self.cursor;
        self.insert(value);
        let end = self.cursor;
        if let Some(attachment_id) = attachment_id {
            self.composer_attachments.push(AttachmentBinding {
                attachment_id,
                start,
                end,
            });
        }
        if trailing_space || self.cursor == self.input.len() {
            self.insert(" ");
        }
        self.close_completion();
    }

    async fn import_clipboard_image_path(&mut self, path: &Path) -> Result<()> {
        let snapshot = self.conversation.snapshot();
        let store = self.attachment_store()?;
        let attachment = store.import_path(self.session_id, &snapshot.session.attachments, path)?;
        let (_, attachment) = self.conversation.add_attachment(attachment).await?;
        let reference = store.visible_reference(&attachment);
        self.insert_clipboard_item(&reference, Some(attachment.id));
        Ok(())
    }

    async fn paste_operating_system_clipboard(&mut self) -> Result<bool> {
        let files = self
            .clipboard
            .as_mut()
            .and_then(|clipboard| clipboard.get().file_list().ok())
            .unwrap_or_default();
        if !files.is_empty() {
            for path in files {
                let is_image = AttachmentStore::path_is_supported_image(&path);
                if is_image {
                    self.import_clipboard_image_path(&path).await?;
                } else {
                    let reference = format!("@{}", path.to_string_lossy());
                    self.insert_clipboard_item(&reference, None);
                }
            }
            return Ok(true);
        }

        let image = self
            .clipboard
            .as_mut()
            .and_then(|clipboard| clipboard.get_image().ok());
        if let Some(image) = image {
            let width = u32::try_from(image.width).context("clipboard image is too wide")?;
            let height = u32::try_from(image.height).context("clipboard image is too tall")?;
            let snapshot = self.conversation.snapshot();
            let store = self.attachment_store()?;
            let attachment = store.import_rgba(
                self.session_id,
                &snapshot.session.attachments,
                width,
                height,
                image.bytes.as_ref(),
            )?;
            let (_, attachment) = self.conversation.add_attachment(attachment).await?;
            let reference = store.visible_reference(&attachment);
            self.insert_clipboard_item(&reference, Some(attachment.id));
            return Ok(true);
        }

        let text = self
            .clipboard
            .as_mut()
            .and_then(|clipboard| clipboard.get_text().ok());
        Ok(text.as_deref().is_some_and(|text| self.handle_paste(text)))
    }

    fn insert_char(&mut self, value: char) {
        let mut text = [0; 4];
        self.insert(value.encode_utf8(&mut text));
    }

    fn refresh_skills(&mut self) {
        match self.conversation.queue_skill_refresh_once(Uuid::new_v4()) {
            Ok(skills) => {
                self.skills = skills
                    .into_iter()
                    .map(|skill| SkillView {
                        name: skill.name,
                        description: skill.description,
                        scope: skill.scope,
                    })
                    .collect();
            }
            Err(error) => self.error = Some(format!("Could not refresh skills: {error:#}")),
        }
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
        self.provider.clone()
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
        let debug_openai = self.debug_openai.clone();
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
        self.replace_composer_range(previous, self.cursor, "");
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
        self.replace_composer_range(self.cursor, next, "");
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
                self.replace_composer_range(start, self.cursor, "");
                self.cursor = start;
            }
            EditorAction::DeleteWordRight => {
                let end = word_right_index(&self.input, self.cursor);
                self.replace_composer_range(self.cursor, end, "");
            }
            EditorAction::MoveLineStart => {
                self.cursor = hard_line_start(&self.input, self.cursor);
            }
            EditorAction::MoveLineEnd => {
                self.cursor = hard_line_end(&self.input, self.cursor);
            }
            EditorAction::DeleteToLineStart => {
                let start = hard_line_start(&self.input, self.cursor);
                self.replace_composer_range(start, self.cursor, "");
                self.cursor = start;
            }
            EditorAction::DeleteToLineEnd => {
                let end = hard_line_end(&self.input, self.cursor);
                self.replace_composer_range(self.cursor, end, "");
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
        self.completion_search = None;
        self.completion_request_id = self.completion_request_id.wrapping_add(1);
        let slash_completion_open = slash_completion_range(&self.input, self.cursor).is_some();
        if slash_completion_open && !self.slash_completion_open {
            self.refresh_skills();
        }
        self.slash_completion_open = slash_completion_open;
        let (menu, search) = complete_progressive(
            &self.input,
            self.cursor,
            &self.project_root,
            self.skills
                .iter()
                .map(|skill| (skill.name.as_str(), skill.description.as_str())),
            self.usage_available(),
            self.completion_request_id,
            self.session_scope == SessionScope::NoProject,
        );
        self.completion = menu;
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
        self.slash_completion_open = false;
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

    async fn accept_completion(&mut self) -> bool {
        let Some(menu) = self.completion.take() else {
            return false;
        };
        self.completion_search = None;
        self.completion_request_id = self.completion_request_id.wrapping_add(1);
        let Some(item) = menu.items.get(menu.selected) else {
            return false;
        };
        let mut replacement = item.replacement.clone();
        let mut attachment_id = None;
        if item.kind == CompletionKind::File {
            let path = PathBuf::from(&item.name);
            let path = if path.is_absolute() {
                path
            } else {
                self.project_root.join(path)
            };
            let is_image = AttachmentStore::path_is_supported_image(&path);
            if is_image {
                let snapshot = self.conversation.snapshot();
                let store = match self.attachment_store() {
                    Ok(store) => store,
                    Err(error) => {
                        self.error = Some(format!("Could not open attachment storage: {error:#}"));
                        return false;
                    }
                };
                let imported = match store.import_path(
                    self.session_id,
                    &snapshot.session.attachments,
                    &path,
                ) {
                    Ok(attachment) => attachment,
                    Err(error) => {
                        self.error = Some(format!("Could not import image: {error:#}"));
                        return false;
                    }
                };
                match self.conversation.add_attachment(imported).await {
                    Ok((_, attachment)) => {
                        replacement = format!("{} ", store.visible_reference(&attachment));
                        attachment_id = Some(attachment.id);
                    }
                    Err(error) => {
                        self.error = Some(format!("Could not import image: {error:#}"));
                        return false;
                    }
                }
            }
        }
        self.replace_composer_range(menu.token_start, menu.token_end, &replacement);
        self.cursor = menu.token_start + replacement.len();
        if let Some(attachment_id) = attachment_id {
            self.composer_attachments.push(AttachmentBinding {
                attachment_id,
                start: menu.token_start,
                end: self.cursor.saturating_sub(1),
            });
        }
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

    fn open_provider_picker(&mut self) {
        let providers = self.config.providers.keys().cloned().collect::<Vec<_>>();
        let selected = providers
            .iter()
            .position(|provider| provider == &self.provider)
            .unwrap_or(0);
        self.provider_picker = Some(ProviderPicker {
            providers,
            selected,
            notice: None,
        });
        self.close_completion();
    }

    fn move_provider_selection(&mut self, delta: isize) {
        let Some(picker) = &mut self.provider_picker else {
            return;
        };
        let len = picker.providers.len();
        if len == 0 {
            return;
        }
        picker.selected = (picker.selected as isize + delta).rem_euclid(len as isize) as usize;
        picker.notice = None;
    }

    async fn accept_provider_selection(&mut self) -> Result<()> {
        let Some(name) = self
            .provider_picker
            .as_ref()
            .and_then(|picker| picker.providers.get(picker.selected).cloned())
        else {
            return Ok(());
        };
        if name == self.provider {
            self.provider_picker = None;
            return Ok(());
        }

        let result = match self.coordinator.build_provider(&name) {
            Ok((provider, configured_model)) => {
                self.conversation
                    .set_provider(name, configured_model, provider)
                    .await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(snapshot) => {
                self.apply_snapshot(snapshot);
                self.sync_usage_provider();
                self.error = None;
                self.provider_picker = None;
            }
            Err(error) => {
                if let Some(picker) = &mut self.provider_picker {
                    picker.notice = Some(format!("{error:#}"));
                }
            }
        }
        Ok(())
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
        let selection = {
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
                        if model.available_service_tiers().is_empty() {
                            picker.service_tier = None;
                            Some(ModelSelection {
                                model: model.slug.clone(),
                                reasoning_effort: picker.reasoning_effort.clone(),
                                service_tier: None,
                            })
                        } else {
                            picker.step = ModelPickerStep::Speed;
                            picker.selected =
                                speed_tier_index(model, picker.service_tier.as_deref());
                            None
                        }
                    } else {
                        picker.step = ModelPickerStep::Reasoning;
                        picker.selected = model
                            .supported_reasoning_levels
                            .iter()
                            .position(|option| {
                                Some(option.effort.as_str()) == picker.reasoning_effort.as_deref()
                            })
                            .unwrap_or(0);
                        None
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
                    if model.available_service_tiers().is_empty() {
                        picker.service_tier = None;
                        Some(ModelSelection {
                            model: model.slug.clone(),
                            reasoning_effort: picker.reasoning_effort.clone(),
                            service_tier: None,
                        })
                    } else {
                        picker.step = ModelPickerStep::Speed;
                        picker.selected = speed_tier_index(model, picker.service_tier.as_deref());
                        None
                    }
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
                    Some(ModelSelection {
                        model: model.slug.clone(),
                        reasoning_effort: picker.reasoning_effort.clone(),
                        service_tier,
                    })
                }
            }
        };
        if let Some(selection) = selection {
            let snapshot = self.conversation.set_model(selection).await?;
            self.apply_snapshot(snapshot);
            self.error = None;
            self.model_picker = None;
        }
        Ok(())
    }

    async fn save_active_session(&self) -> Result<()> {
        if self.active_session {
            self.conversation.persist_if_idle().await?;
        }
        Ok(())
    }

    fn apply_snapshot(&mut self, snapshot: ConversationSnapshot) {
        if self.provider != snapshot.session.provider {
            self.model_catalog.clone_from(&snapshot.model_catalog);
            self.model_catalogs
                .insert(snapshot.session.id, snapshot.model_catalog.clone());
        }
        self.transcript = snapshot.session.messages.to_vec();
        self.transcript_node_ids = snapshot.session.messages.active_node_ids().to_vec();
        self.activities.clone_from(&snapshot.session.activities);
        self.turns.clone_from(&snapshot.session.turns);
        self.goals.clone_from(&snapshot.session.goals);
        self.visible_goal_id = snapshot.session.visible_goal_id;
        self.session_id = snapshot.session.id;
        self.project_root = snapshot.project_root;
        self.session_scope = snapshot.session.scope;
        self.project = match self.session_scope {
            SessionScope::Project => self.project_root.display().to_string(),
            SessionScope::NoProject => "No project".into(),
        };
        self.provider.clone_from(&snapshot.session.provider);
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
        self.provider_picker = None;
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
        let selected_row = navigator
            .rows
            .iter()
            .position(|row| row.id == selected_id)
            .unwrap_or(0);
        let target = match direction {
            KeyCode::Up => selected_row
                .checked_sub(1)
                .and_then(|row| navigator.rows.get(row))
                .and_then(|row| navigator.nodes.iter().position(|node| node.id == row.id)),
            KeyCode::Down => navigator
                .rows
                .get(selected_row + 1)
                .and_then(|row| navigator.nodes.iter().position(|node| node.id == row.id)),
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

    fn begin_selected_message_edit(&mut self) {
        let Some(navigator) = self.branch_navigator.as_ref() else {
            return;
        };
        let node_id = navigator.nodes[navigator.selected].id;
        let snapshot = self.conversation.snapshot();
        let Some(message) = snapshot.session.messages.message(node_id) else {
            self.error = Some("Only visible user messages can be edited.".into());
            return;
        };
        let Some(content) = visible_message_content(message).map(str::to_owned) else {
            self.error = Some("Only visible user messages can be edited.".into());
            return;
        };
        let attachments = attachment_bindings_for_message(message);
        let previous_input = std::mem::take(&mut self.input);
        let previous_cursor = self.cursor;
        let previous_attachments = std::mem::take(&mut self.composer_attachments);
        self.branch_navigator = None;
        self.pending_branch_node = None;
        self.input = content;
        self.composer_attachments = attachments;
        self.cursor = self.input.len();
        self.preferred_column = None;
        self.close_completion();
        self.editing_message = Some(MessageEdit {
            node_id,
            previous_input,
            previous_cursor,
            previous_attachments,
        });
        self.error = None;
    }

    fn cancel_message_edit(&mut self) {
        let Some(edit) = self.editing_message.take() else {
            return;
        };
        self.input = edit.previous_input;
        self.composer_attachments = edit.previous_attachments;
        self.cursor = edit.previous_cursor.min(self.input.len());
        self.preferred_column = None;
        self.close_completion();
        let snapshot = self.conversation.snapshot();
        self.apply_branch_session(&snapshot.session);
        self.auto_scroll = true;
        self.error = None;
    }

    fn recall_latest_user_message(&mut self) {
        if self.is_busy() || !self.input.is_empty() || self.editing_message.is_some() {
            return;
        }
        let Some((message_index, message, content)) = self
            .transcript
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, message)| {
                matches!(message.role, Role::User)
                    .then(|| {
                        visible_message_content(message).map(|content| (index, message, content))
                    })
                    .flatten()
            })
        else {
            return;
        };
        let Some(node_id) = self.transcript_node_ids.get(message_index).copied() else {
            return;
        };
        self.input = content.to_owned();
        self.composer_attachments = attachment_bindings_for_message(message);
        self.cursor = self.input.len();
        self.preferred_column = None;
        self.close_completion();
        self.editing_message = Some(MessageEdit {
            node_id,
            previous_input: String::new(),
            previous_cursor: 0,
            previous_attachments: Vec::new(),
        });
        self.error = None;
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
        self.provider_picker = None;
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
            self.start_turn(prompt, Vec::new(), false)?;
            return Ok(true);
        }
        if continue_goal {
            self.start_goal_continuation()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn open_processes(&mut self) {
        let processes = self.conversation.running_terminals();
        self.process_dialog = Some(ProcessDialog {
            processes,
            selected: 0,
            view: ProcessDialogView::List,
            output: None,
            output_scroll: 0,
            output_follow: true,
            stop_confirm: false,
            last_refresh: Instant::now() - Duration::from_secs(1),
        });
        self.show_help = false;
        self.show_skills = false;
        self.model_picker = None;
        self.provider_picker = None;
        self.session_picker = None;
        self.goal_picker = None;
        self.close_completion();
        self.refresh_process_dialog();
    }

    fn refresh_process_dialog(&mut self) {
        let Some(dialog) = self.process_dialog.as_mut() else {
            return;
        };
        if dialog.last_refresh.elapsed() < Duration::from_millis(100) {
            return;
        }
        dialog.last_refresh = Instant::now();
        let selected_id = dialog
            .processes
            .get(dialog.selected)
            .map(|terminal| terminal.id.clone());
        dialog.processes = self.conversation.running_terminals();
        dialog.selected = selected_id
            .as_ref()
            .and_then(|id| {
                dialog
                    .processes
                    .iter()
                    .position(|terminal| &terminal.id == id)
            })
            .unwrap_or_else(|| {
                dialog
                    .selected
                    .min(dialog.processes.len().saturating_sub(1))
            });
        if let ProcessDialogView::Output(terminal_id) = &dialog.view {
            match self.conversation.terminal_output(terminal_id) {
                Ok(output) => dialog.output = Some(output),
                Err(error) => {
                    self.error = Some(format!("Could not read terminal output: {error:#}"))
                }
            }
        }
    }

    fn selected_process_id(&self) -> Option<String> {
        let dialog = self.process_dialog.as_ref()?;
        match &dialog.view {
            ProcessDialogView::List => dialog
                .processes
                .get(dialog.selected)
                .map(|terminal| terminal.id.clone()),
            ProcessDialogView::Output(terminal_id) => Some(terminal_id.clone()),
        }
    }

    fn view_selected_process(&mut self) {
        let Some(terminal_id) = self.selected_process_id() else {
            return;
        };
        let output = match self.conversation.terminal_output(&terminal_id) {
            Ok(output) => output,
            Err(error) => {
                self.error = Some(format!("Could not read terminal output: {error:#}"));
                return;
            }
        };
        if let Some(dialog) = self.process_dialog.as_mut() {
            dialog.view = ProcessDialogView::Output(terminal_id);
            dialog.output = Some(output);
            dialog.output_scroll = 0;
            dialog.output_follow = true;
        }
    }

    fn go_to_selected_process_origin(&mut self) {
        let origin = self.process_dialog.as_ref().and_then(|dialog| {
            dialog
                .output
                .as_ref()
                .and_then(|output| output.origin_activity_id.clone())
                .or_else(|| {
                    dialog
                        .processes
                        .get(dialog.selected)
                        .and_then(|terminal| terminal.origin_activity_id.clone())
                })
        });
        let Some(origin) = origin else {
            self.error = Some("This terminal has no recorded origin activity.".into());
            return;
        };
        if let Some(activity) = self
            .activities
            .iter()
            .find(|activity| activity.id == origin)
        {
            self.expanded_turns.insert(TurnKey {
                session_id: self.session_id,
                message_index: activity.turn_message_index,
            });
        }
        self.process_dialog = None;
        self.pending_activity_id = Some(origin);
        self.auto_scroll = false;
    }

    fn stop_selected_process(&mut self) {
        let Some(terminal_id) = self.selected_process_id() else {
            return;
        };
        if let Err(error) = self.conversation.close_terminal(&terminal_id) {
            self.error = Some(format!("Could not stop terminal: {error:#}"));
            return;
        }
        if let Some(dialog) = self.process_dialog.as_mut() {
            dialog.stop_confirm = false;
            if matches!(dialog.view, ProcessDialogView::Output(_)) {
                dialog.output = self.conversation.terminal_output(&terminal_id).ok();
            }
            dialog.last_refresh = Instant::now() - Duration::from_secs(1);
        }
        self.refresh_process_dialog();
    }

    fn request_quit(&mut self) {
        if self.conversations.active_terminal_count() > 0 {
            self.exit_confirm = true;
        } else {
            self.should_quit = true;
        }
    }

    fn handle_process_key(&mut self, key: KeyCode) {
        let confirming = self
            .process_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.stop_confirm);
        if confirming {
            match key {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.stop_selected_process(),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    if let Some(dialog) = self.process_dialog.as_mut() {
                        dialog.stop_confirm = false;
                    }
                }
                _ => {}
            }
            return;
        }
        let output_open = self
            .process_dialog
            .as_ref()
            .is_some_and(|dialog| matches!(dialog.view, ProcessDialogView::Output(_)));
        if output_open {
            match key {
                KeyCode::Up => {
                    if let Some(dialog) = self.process_dialog.as_mut() {
                        dialog.output_follow = false;
                        dialog.output_scroll = dialog.output_scroll.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
                    if let Some(dialog) = self.process_dialog.as_mut() {
                        dialog.output_follow = false;
                        dialog.output_scroll = dialog.output_scroll.saturating_add(1);
                    }
                }
                KeyCode::PageUp => {
                    if let Some(dialog) = self.process_dialog.as_mut() {
                        dialog.output_follow = false;
                        dialog.output_scroll = dialog.output_scroll.saturating_sub(10);
                    }
                }
                KeyCode::PageDown => {
                    if let Some(dialog) = self.process_dialog.as_mut() {
                        dialog.output_follow = false;
                        dialog.output_scroll = dialog.output_scroll.saturating_add(10);
                    }
                }
                KeyCode::End => {
                    if let Some(dialog) = self.process_dialog.as_mut() {
                        dialog.output_follow = true;
                    }
                }
                KeyCode::Char('g' | 'G') => self.go_to_selected_process_origin(),
                KeyCode::Char('k' | 'K') | KeyCode::Delete => {
                    if let Some(dialog) = self.process_dialog.as_mut() {
                        dialog.stop_confirm = true;
                    }
                }
                KeyCode::Esc => {
                    if let Some(dialog) = self.process_dialog.as_mut() {
                        dialog.view = ProcessDialogView::List;
                        dialog.output = None;
                    }
                }
                _ => {}
            }
            return;
        }
        match key {
            KeyCode::Up => {
                if let Some(dialog) = self.process_dialog.as_mut()
                    && !dialog.processes.is_empty()
                {
                    dialog.selected = dialog.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(dialog) = self.process_dialog.as_mut()
                    && !dialog.processes.is_empty()
                {
                    dialog.selected =
                        (dialog.selected + 1).min(dialog.processes.len().saturating_sub(1));
                }
            }
            KeyCode::Enter | KeyCode::Char('o' | 'O') => self.view_selected_process(),
            KeyCode::Char('g' | 'G') => self.go_to_selected_process_origin(),
            KeyCode::Char('k' | 'K') | KeyCode::Delete => {
                if self.selected_process_id().is_some()
                    && let Some(dialog) = self.process_dialog.as_mut()
                {
                    dialog.stop_confirm = true;
                }
            }
            KeyCode::Esc => self.process_dialog = None,
            _ => {}
        }
    }

    async fn open_session_picker(&mut self) -> Result<()> {
        self.save_active_session().await?;
        let current_id = self
            .active_session
            .then(|| self.conversation.snapshot().session.id);
        let projects = list_session_projects(&self.project_root, &self.registry)?
            .into_iter()
            .map(|project| {
                let expanded = match (&project.root, self.session_scope) {
                    (None, SessionScope::NoProject) => true,
                    (Some(root), SessionScope::Project) => paths_equal(root, &self.project_root),
                    _ => false,
                };
                let archived_expanded = current_id.is_some_and(|id| {
                    project
                        .archived_sessions()
                        .iter()
                        .any(|session| session.id == id)
                });
                SessionProjectView::new(project, expanded, archived_expanded)
            })
            .collect::<Vec<_>>();
        let mut picker = SessionPicker {
            projects,
            selected: 0,
            collapsed_sessions: HashSet::new(),
        };
        if let Some(selected) = picker.rows().iter().position(|row| {
            let SessionPickerRow::Session(project, section, session) = *row else {
                return false;
            };
            section != SessionSection::Pinned
                && Some(picker.projects[project].sessions(section)[session].id) == current_id
        }) {
            picker.selected = selected;
        }
        self.session_picker = Some(picker);
        self.show_help = false;
        self.show_skills = false;
        self.model_picker = None;
        self.provider_picker = None;
        self.close_completion();
        Ok(())
    }

    fn rebuild_session_picker(&mut self, selected_id: Uuid, expand_archive: bool) -> Result<()> {
        let Some(existing) = self.session_picker.as_ref() else {
            return Ok(());
        };
        let expanded = existing
            .projects
            .iter()
            .filter(|project| project.expanded)
            .map(|project| project.project.root.clone())
            .collect::<Vec<_>>();
        let archived_expanded = existing
            .projects
            .iter()
            .filter(|project| project.archived_expanded)
            .map(|project| project.project.root.clone())
            .collect::<Vec<_>>();
        let collapsed_sessions = existing.collapsed_sessions.clone();
        let projects = list_session_projects(&self.project_root, &self.registry)?;
        let mut picker = SessionPicker {
            projects: projects
                .into_iter()
                .map(|project| {
                    let is_expanded = expanded.iter().any(|root| match (root, &project.root) {
                        (Some(left), Some(right)) => paths_equal(left, right),
                        (None, None) => true,
                        _ => false,
                    });
                    let target_archived = project
                        .archived_sessions()
                        .iter()
                        .any(|session| session.id == selected_id);
                    let archive_is_expanded = (expand_archive && target_archived)
                        || archived_expanded
                            .iter()
                            .any(|root| match (root, &project.root) {
                                (Some(left), Some(right)) => paths_equal(left, right),
                                (None, None) => true,
                                _ => false,
                            });
                    SessionProjectView::new(project, is_expanded, archive_is_expanded)
                })
                .collect(),
            selected: 0,
            collapsed_sessions,
        };
        for project in 0..picker.projects.len() {
            picker.select_session(project, selected_id);
            if picker
                .selected_session()
                .is_some_and(|(_, _, _, session)| session.id == selected_id)
            {
                break;
            }
        }
        self.session_picker = Some(picker);
        Ok(())
    }

    async fn update_selected_session_metadata(
        &mut self,
        update: SessionMetadataUpdate,
        expand_archive: bool,
    ) -> Result<()> {
        let Some((project_index, _, _, session)) = self
            .session_picker
            .as_ref()
            .and_then(SessionPicker::selected_session)
        else {
            return Ok(());
        };
        let id = session.id;
        let root = self.session_picker.as_ref().unwrap().projects[project_index]
            .project
            .root
            .clone();
        if let Some(conversation) = self.conversations.get(id) {
            conversation.update_metadata(update).await?;
        } else {
            let store =
                SessionStore::for_project_root_in(root.as_deref(), &self.registry.data_dir()?)?;
            let mut session = store.load(Some(&id.to_string()))?;
            session.update_metadata(update, chrono::Utc::now())?;
            store.save(&session)?;
        }
        self.rebuild_session_picker(id, expand_archive)?;
        Ok(())
    }

    fn begin_session_rename(&mut self) {
        let Some((project_index, _, _, session)) = self
            .session_picker
            .as_ref()
            .and_then(SessionPicker::selected_session)
        else {
            return;
        };
        self.session_rename = Some(SessionRename {
            project_index,
            session_id: session.id,
            title: session.title.clone(),
        });
    }

    async fn finish_session_rename(&mut self) -> Result<()> {
        let Some(rename) = self.session_rename.take() else {
            return Ok(());
        };
        let title = rename.title.trim().to_owned();
        if title.is_empty() {
            self.error = Some("Session title cannot be empty.".into());
            return Ok(());
        }
        if title.chars().count() > 120 {
            self.error = Some("Session title cannot exceed 120 characters.".into());
            return Ok(());
        }
        let Some(picker) = self.session_picker.as_mut() else {
            return Ok(());
        };
        picker.select_session(rename.project_index, rename.session_id);
        self.update_selected_session_metadata(SessionMetadataUpdate::Rename(title), false)
            .await
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
            Some(SessionPickerRow::Session(project, section, session_index)) => {
                let session = &picker.projects[project].sessions(section)[session_index];
                let id = session.id;
                let parent = session.parent_session_id;
                let depth = session.depth;
                if session.descendant_count > 0 && !picker.collapsed_sessions.contains(&id) {
                    picker.collapsed_sessions.insert(id);
                    picker.select_session(project, id);
                } else if depth > 0 {
                    if let Some(parent) = parent {
                        picker.select_session(project, parent);
                    }
                } else {
                    picker.select_project(project);
                }
            }
            Some(SessionPickerRow::Section(project, SessionSection::Archived)) => {
                picker.projects[project].archived_expanded = false;
            }
            Some(SessionPickerRow::Section(project, _)) => picker.select_project(project),
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
        match picker.selected_row() {
            Some(SessionPickerRow::Project(project)) if !picker.projects[project].expanded => {
                picker.projects[project].expanded = true;
                picker.select_project(project);
            }
            Some(SessionPickerRow::Section(project, SessionSection::Archived)) => {
                picker.projects[project].archived_expanded = true;
            }
            Some(SessionPickerRow::Session(project, section, session_index)) => {
                let id = picker.projects[project].sessions(section)[session_index].id;
                if picker.collapsed_sessions.remove(&id) {
                    picker.select_session(project, id);
                }
            }
            _ => {}
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
        let SessionPickerRow::Session(project_index, section, session_index) = row else {
            self.move_session_right();
            return Ok(());
        };
        let (root, id) = {
            let picker = self.session_picker.as_ref().unwrap();
            let project = &picker.projects[project_index].project;
            (
                project.root.clone(),
                picker.projects[project_index].sessions(section)[session_index].id,
            )
        };
        if self.active_session {
            let current_id = self.conversation.snapshot().session.id;
            if current_id != id {
                self.conversations
                    .prepare_for_navigation(current_id)
                    .await?;
            }
        }
        self.park_current_turn();
        let mut catalog_error = None;
        if self.active_session {
            self.model_catalogs.insert(
                self.conversation.snapshot().session.id,
                self.model_catalog.clone(),
            );
        }
        self.conversation = if let Some(existing) = self.conversations.get(id) {
            self.model_catalogs
                .entry(id)
                .or_insert_with(|| existing.snapshot().model_catalog);
            existing
        } else {
            let session =
                SessionStore::for_project_root_in(root.as_deref(), &self.registry.data_dir()?)?
                    .load(Some(&id.to_string()))?;
            let execution_root = root.as_deref().unwrap_or(&self.runtime_root);
            let mut agent = self.coordinator.build_agent(execution_root, session)?;
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
        if !self.active_session {
            return;
        }
        if self.running.is_none()
            && self.prompt_queue.items.is_empty()
            && self.queued_prompt_edit.is_none()
            && !self.resume_goal_after_queue
        {
            return;
        }
        let queued_prompt_edit = self.queued_prompt_edit.take();
        if let Some(edit) = &queued_prompt_edit {
            self.input.clone_from(&edit.previous_input);
            self.composer_attachments
                .clone_from(&edit.previous_attachments);
            self.cursor = edit.previous_cursor.min(self.input.len());
            self.preferred_column = None;
            self.close_completion();
        }
        let running = self.running.take();
        let event_rx = self.event_rx.take();
        debug_assert_eq!(running.is_some(), event_rx.is_some());
        let snapshot = self.conversation.snapshot();
        let id = snapshot.session.id;
        self.background_turns.insert(
            id,
            BackgroundTurn {
                provider: snapshot.session.provider,
                running,
                event_rx,
                pending_user: self.pending_user.take(),
                pending_user_attachments: std::mem::take(&mut self.pending_user_attachments),
                prompt_queue: std::mem::take(&mut self.prompt_queue),
                queued_prompt_edit,
                resume_goal_after_queue: std::mem::take(&mut self.resume_goal_after_queue),
                pending_goal_action: self.pending_goal_action.take(),
                live_messages: std::mem::take(&mut self.live_messages),
                usage_refresh_requested: std::mem::take(&mut self.running_usage_refresh_requested),
            },
        );
    }

    async fn refresh_usage_for_finished_background_turns(&mut self) {
        let mut provider = None;
        for background in self.background_turns.values_mut() {
            if background.usage_refresh_requested
                || !background
                    .running
                    .as_ref()
                    .is_some_and(JoinHandle::is_finished)
            {
                continue;
            }
            let handle = background.running.take().expect("checked above");
            let turn =
                match handle.await {
                    Ok(turn) => turn,
                    Err(error) => Err(anyhow::Error::new(error)
                        .context("background conversation turn task failed")),
                };
            let turn_succeeded = turn.as_ref().is_ok_and(|turn| turn.result.is_ok());
            background.running = Some(tokio::spawn(async move { turn }));
            background.usage_refresh_requested = true;
            if turn_succeeded
                && self
                    .usage_tracker
                    .available_for(&self.config, &background.provider)
            {
                provider = Some(background.provider.clone());
            }
        }
        let Some(provider) = provider else {
            return;
        };
        if self.usage_available() {
            self.start_usage_refresh();
        } else {
            self.usage_tracker
                .refresh_in_background(self.config.clone(), provider);
        }
    }

    fn restore_turn_state(&mut self, id: Uuid) {
        let Some(background) = self.background_turns.remove(&id) else {
            return;
        };
        self.running = background.running;
        self.event_rx = background.event_rx;
        self.pending_user = background.pending_user;
        self.pending_user_attachments = background.pending_user_attachments;
        self.prompt_queue = background.prompt_queue;
        self.queued_prompt_edit = background.queued_prompt_edit;
        self.resume_goal_after_queue = background.resume_goal_after_queue;
        if let Some(edit) = &self.queued_prompt_edit
            && let Some(prompt) = self
                .prompt_queue
                .items
                .iter()
                .find(|prompt| prompt.id == edit.id)
        {
            self.input.clone_from(&prompt.content);
            self.composer_attachments.clone_from(&prompt.attachments);
            self.cursor = self.input.len();
            self.preferred_column = None;
            self.close_completion();
        }
        self.pending_goal_action = background.pending_goal_action;
        self.live_messages = background.live_messages;
        self.running_usage_refresh_requested = background.usage_refresh_requested;
    }

    async fn delete_session_selection(&mut self) -> Result<()> {
        let Some((project_index, _, _, selected_session)) = self
            .session_picker
            .as_ref()
            .and_then(SessionPicker::selected_session)
        else {
            return Ok(());
        };
        let selected = self.session_picker.as_ref().unwrap().selected;
        let (root, id) = {
            let picker = self.session_picker.as_ref().unwrap();
            let project = &picker.projects[project_index].project;
            (project.root.clone(), selected_session.id)
        };
        let active_terminals = self
            .conversations
            .get(id)
            .map(|conversation| conversation.running_terminal_count())
            .unwrap_or(0);
        if active_terminals > 0 && !self.session_delete_confirm {
            self.session_delete_confirm = true;
            return Ok(());
        }
        self.session_delete_confirm = false;
        if active_terminals > 0
            && let Some(conversation) = self.conversations.get(id)
        {
            conversation.close_terminals()?;
        }
        let store = SessionStore::for_project_root_in(root.as_deref(), &self.registry.data_dir()?)?;
        let active_snapshot = self.conversation.snapshot();
        let deleting_active = self.active_session
            && id == active_snapshot.session.id
            && match (&root, active_snapshot.session.scope) {
                (None, SessionScope::NoProject) => true,
                (Some(root), SessionScope::Project) => {
                    paths_equal(root, &active_snapshot.project_root)
                }
                _ => false,
            };
        if let Some(conversation) = self.conversations.take_if_idle(id)? {
            conversation.shutdown().await?;
        }
        self.background_turns.remove(&id);
        self.model_catalogs.remove(&id);
        store.discard(id)?;

        if deleting_active {
            if let Some(summary) = store.list()?.first() {
                self.conversation = if let Some(existing) = self.conversations.get(summary.id) {
                    existing
                } else {
                    let session = store.load(Some(&summary.id.to_string()))?;
                    let execution_root = root.as_deref().unwrap_or(&self.runtime_root);
                    let agent = self.coordinator.build_agent(execution_root, session)?;
                    self.conversations.install(agent)?
                };
                self.model_catalog = self
                    .model_catalogs
                    .get(&summary.id)
                    .cloned()
                    .unwrap_or_default();
                self.sync_active_session();
            } else {
                self.active_session = false;
                self.transcript.clear();
                self.transcript_node_ids.clear();
                self.live_messages.clear();
                self.activities.clear();
                self.turns.clear();
                self.goals.clear();
                self.visible_goal_id = None;
            }
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
        let collapsed_sessions = self
            .session_picker
            .as_ref()
            .map(|picker| picker.collapsed_sessions.clone())
            .unwrap_or_default();
        let archived_expanded = self
            .session_picker
            .as_ref()
            .map(|picker| {
                picker
                    .projects
                    .iter()
                    .filter(|project| project.archived_expanded)
                    .map(|project| project.project.root.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut picker = SessionPicker {
            projects: projects
                .into_iter()
                .map(|project| {
                    let is_expanded = expanded.iter().any(|root| match (root, &project.root) {
                        (Some(left), Some(right)) => paths_equal(left, right),
                        (None, None) => true,
                        _ => false,
                    });
                    let archive_is_expanded =
                        archived_expanded
                            .iter()
                            .any(|root| match (root, &project.root) {
                                (Some(left), Some(right)) => paths_equal(left, right),
                                (None, None) => true,
                                _ => false,
                            });
                    SessionProjectView::new(project, is_expanded, archive_is_expanded)
                })
                .collect(),
            selected: 0,
            collapsed_sessions,
        };
        picker.selected = selected.min(picker.rows().len().saturating_sub(1));
        self.session_picker = Some(picker);
        self.error =
            (!self.active_session).then(|| "No saved sessions remain in this group.".to_owned());
        Ok(())
    }

    fn sync_active_session(&mut self) {
        self.active_session = true;
        self.apply_snapshot(self.conversation.snapshot());
        self.sync_usage_provider();
        self.live_messages.clear();
        self.pending_user = None;
        self.pending_user_attachments.clear();
        self.prompt_queue = PromptQueue::default();
        self.queued_prompt_edit = None;
        self.resume_goal_after_queue = false;
        self.goal_picker = None;
        self.pending_goal_action = None;
        self.editing_goal_id = None;
        self.editing_goal_resume = false;
        self.editing_message = None;
        self.goal_buttons = GoalButtons::default();
        self.last_escape = None;
        self.queued_prompt_buttons.clear();
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

    fn begin_queued_prompt_edit(&mut self, id: u64) {
        if self
            .queued_prompt_edit
            .as_ref()
            .is_some_and(|edit| edit.id == id)
        {
            return;
        }
        self.cancel_queued_prompt_edit();
        let Some((content, attachments)) = self
            .prompt_queue
            .items
            .iter()
            .find(|prompt| prompt.id == id)
            .map(|prompt| (prompt.content.clone(), prompt.attachments.clone()))
        else {
            return;
        };
        self.queued_prompt_edit = Some(QueuedPromptEdit {
            id,
            previous_input: std::mem::take(&mut self.input),
            previous_cursor: self.cursor,
            previous_attachments: std::mem::take(&mut self.composer_attachments),
        });
        self.input = content;
        self.composer_attachments = attachments;
        self.cursor = self.input.len();
        self.preferred_column = None;
        self.close_completion();
        self.error = None;
    }

    fn cancel_queued_prompt_edit(&mut self) {
        let Some(edit) = self.queued_prompt_edit.take() else {
            return;
        };
        self.input = edit.previous_input;
        self.composer_attachments = edit.previous_attachments;
        self.cursor = edit.previous_cursor.min(self.input.len());
        self.preferred_column = None;
        self.close_completion();
        self.error = None;
    }

    fn finish_queued_prompt_edit(&mut self, content: String) {
        let Some(edit) = self.queued_prompt_edit.take() else {
            return;
        };
        let attachments = std::mem::take(&mut self.composer_attachments);
        if !self.prompt_queue.update(edit.id, content, attachments) {
            self.error = Some("The queued message no longer exists.".into());
        } else {
            self.error = None;
        }
        self.input = edit.previous_input;
        self.composer_attachments = edit.previous_attachments;
        self.cursor = edit.previous_cursor.min(self.input.len());
        self.preferred_column = None;
        self.close_completion();
    }

    fn delete_queued_prompt(&mut self, id: u64) {
        if self
            .queued_prompt_edit
            .as_ref()
            .is_some_and(|edit| edit.id == id)
        {
            self.cancel_queued_prompt_edit();
        }
        self.prompt_queue.remove(id);
    }

    fn steer_queued_prompt(&mut self, id: u64) {
        if self
            .queued_prompt_edit
            .as_ref()
            .is_some_and(|edit| edit.id == id)
        {
            return;
        }
        if self.prompt_queue.steer(id) && self.is_running() {
            self.cancel_current_turn();
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.goal_picker.is_none()
            && self.session_picker.is_none()
            && self.process_dialog.is_none()
            && !self.session_delete_confirm
            && !self.exit_confirm
            && self.model_picker.is_none()
            && self.provider_picker.is_none()
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
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            let point = (mouse.column, mouse.row).into();
            let action = self.queued_prompt_buttons.iter().find_map(|buttons| {
                if buttons.steer.contains(point) {
                    Some(QueuedPromptAction::Steer(buttons.id))
                } else if buttons.edit.contains(point) {
                    Some(QueuedPromptAction::Edit(buttons.id))
                } else if buttons.delete.contains(point) {
                    Some(QueuedPromptAction::Delete(buttons.id))
                } else {
                    None
                }
            });
            match action {
                Some(QueuedPromptAction::Steer(id)) => self.steer_queued_prompt(id),
                Some(QueuedPromptAction::Edit(id)) => self.begin_queued_prompt_edit(id),
                Some(QueuedPromptAction::Delete(id)) => self.delete_queued_prompt(id),
                None => {}
            }
            if action.is_some() {
                return;
            }
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
            MouseEventKind::ScrollUp if self.provider_picker.is_some() => {
                self.move_provider_selection(-1);
            }
            MouseEventKind::ScrollDown if self.provider_picker.is_some() => {
                self.move_provider_selection(1);
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
        let editing_id = self.queued_prompt_edit.as_ref().map(|edit| edit.id);
        let handle = self.running.take().expect("checked above");
        let turn = handle.await.context("conversation turn task failed")??;
        let current_skills = std::mem::take(&mut self.skills);
        self.apply_snapshot(turn.snapshot);
        self.skills = current_skills;
        self.event_rx = None;
        self.last_escape = None;
        self.live_messages.clear();
        let pending_user = self.pending_user.take();
        let pending_attachments = std::mem::take(&mut self.pending_user_attachments);
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
                if self.input.is_empty()
                    && let Some(prompt) = pending_user
                {
                    self.input = prompt;
                    self.composer_attachments = pending_attachments;
                    self.cursor = self.input.len();
                }
                false
            }
        };
        if turn_succeeded && !std::mem::take(&mut self.running_usage_refresh_requested) {
            self.start_usage_refresh();
        }
        if self.apply_pending_goal_action().await? {
            return Ok(());
        }
        if let Some(prompt) = self.prompt_queue.pop_next(editing_id) {
            self.start_turn(prompt.content, prompt.attachments, false)?;
        } else if turn_succeeded
            && self.prompt_queue.items.is_empty()
            && self.active_goal_id().is_some()
        {
            self.resume_goal_after_queue = false;
            self.start_goal_continuation()?;
        } else if turn_succeeded && self.active_goal_id().is_some() {
            self.resume_goal_after_queue = true;
        }
        Ok(())
    }

    fn dispatch_queued_prompt_if_idle(&mut self) -> Result<bool> {
        if self.is_running() {
            return Ok(false);
        }
        let editing_id = self.queued_prompt_edit.as_ref().map(|edit| edit.id);
        if let Some(prompt) = self.prompt_queue.pop_next(editing_id) {
            self.start_turn(prompt.content, prompt.attachments, false)?;
            return Ok(true);
        }
        if self.prompt_queue.items.is_empty()
            && self.resume_goal_after_queue
            && self.active_goal_id().is_some()
        {
            self.resume_goal_after_queue = false;
            self.start_goal_continuation()?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn submit(&mut self) -> Result<()> {
        if self.recording.is_some() || self.transcription.is_some() {
            return Ok(());
        }
        let (prompt, attachments) = self.trimmed_composer();
        if prompt.is_empty() {
            if self.queued_prompt_edit.is_some() {
                self.error = Some("A queued message cannot be empty; delete it instead.".into());
            }
            return Ok(());
        }
        if !self.active_session {
            if matches!(
                prompt.as_str(),
                "/sessions" | "/no-project" | "/help" | "/quit"
            ) {
                self.clear_composer_text();
                self.preferred_column = None;
                self.close_completion();
                return self.command(&prompt).await;
            }
            self.error = Some(
                "There is no active session. Resume one with /sessions or create one with /no-project."
                    .into(),
            );
            return Ok(());
        }
        if self.queued_prompt_edit.is_some() {
            self.finish_queued_prompt_edit(prompt);
            return Ok(());
        }
        if let Some(edit) = self.editing_message.take() {
            return self.start_message_edit_turn(edit.node_id, prompt, attachments);
        }
        if let Some(id) = self.editing_goal_id.take() {
            if prompt.chars().count() > 4_000 {
                self.editing_goal_id = Some(id);
                self.error = Some("Goal objective cannot exceed 4,000 characters.".into());
                return Ok(());
            }
            let resume = std::mem::take(&mut self.editing_goal_resume);
            if let Some(snapshot) = self.conversation.edit_goal(id, prompt, resume).await? {
                self.clear_composer_text();
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
            self.clear_composer_text();
            self.preferred_column = None;
            self.close_completion();
            self.request_goal_action(PendingGoalAction::Create(objective));
            if !self.is_running() {
                self.apply_pending_goal_action().await?;
            }
            return Ok(());
        }
        if prompt == "/goals" {
            self.clear_composer_text();
            self.preferred_column = None;
            self.close_completion();
            return self.command(&prompt).await;
        }
        if prompt == "/processes" {
            self.clear_composer_text();
            self.preferred_column = None;
            self.close_completion();
            self.open_processes();
            return Ok(());
        }
        if prompt == "/usage" && self.usage_available() {
            self.clear_composer_text();
            self.preferred_column = None;
            self.close_completion();
            return self.command(&prompt).await;
        }
        if prompt == "/models" || prompt == "/providers" {
            self.clear_composer_text();
            self.preferred_column = None;
            self.close_completion();
            if self.is_running() {
                self.error =
                    Some("Wait for the active turn before changing the model or provider.".into());
            } else if prompt == "/models" {
                self.open_model_picker();
            } else {
                self.open_provider_picker();
            }
            return Ok(());
        }
        if prompt == "/cron" || prompt.starts_with("/cron ") {
            self.clear_composer_text();
            self.preferred_column = None;
            self.close_completion();
            return self.cron_command(&prompt).await;
        }
        if self.is_running() {
            self.clear_composer_text();
            self.preferred_column = None;
            self.close_completion();
            self.prompt_queue.push(prompt, attachments);
            return Ok(());
        }
        if builtin_command_from_input(&self.input).is_some() {
            self.clear_composer_text();
            self.preferred_column = None;
            self.close_completion();
            return self.command(&prompt).await;
        }

        self.start_turn(prompt, attachments, true)?;
        self.auto_scroll = true;
        Ok(())
    }

    fn start_turn(
        &mut self,
        prompt: String,
        attachments: Vec<AttachmentBinding>,
        clear_composer: bool,
    ) -> Result<()> {
        if clear_composer {
            self.clear_composer_text();
            self.preferred_column = None;
            self.close_completion();
        }
        self.error = None;
        self.pending_user = Some(prompt.clone());
        self.pending_user_attachments = attachments.clone();
        self.live_messages.clear();
        self.running_usage_refresh_requested = false;

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_rx = Some(event_rx);
        self.running = Some(self.conversation.start_turn_with_attachments(
            prompt,
            attachments,
            Some(event_tx),
        )?);
        Ok(())
    }

    fn start_message_edit_turn(
        &mut self,
        node_id: Uuid,
        prompt: String,
        attachments: Vec<AttachmentBinding>,
    ) -> Result<()> {
        let Some(message_index) = self
            .transcript_node_ids
            .iter()
            .position(|candidate| *candidate == node_id)
        else {
            self.error = Some("The message is no longer on the visible branch.".into());
            return Ok(());
        };
        self.transcript.truncate(message_index);
        self.transcript_node_ids.truncate(message_index);
        self.activities
            .retain(|activity| activity.turn_message_index < message_index);
        self.turns.retain(|turn| turn.message_index < message_index);
        self.clear_composer_text();
        self.preferred_column = None;
        self.close_completion();
        self.error = None;
        self.pending_user = Some(prompt.clone());
        self.pending_user_attachments = attachments.clone();
        self.live_messages.clear();
        self.running_usage_refresh_requested = false;

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_rx = Some(event_rx);
        self.running = Some(self.conversation.start_edit_turn_with_attachments(
            node_id,
            prompt,
            attachments,
            Some(event_tx),
        )?);
        Ok(())
    }

    fn start_goal_continuation(&mut self) -> Result<()> {
        self.error = None;
        self.pending_user = None;
        self.pending_user_attachments.clear();
        self.live_messages.clear();
        self.running_usage_refresh_requested = false;

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_rx = Some(event_rx);
        self.running = Some(self.conversation.start_goal_continuation(Some(event_tx))?);
        Ok(())
    }

    fn cancel_current_turn(&mut self) {
        self.conversation.cancel();
    }

    async fn cron_command(&mut self, command: &str) -> Result<()> {
        if self.is_running() {
            self.error = Some("Wait for the active turn before managing scheduled tasks.".into());
            return Ok(());
        }
        let store = CronStore::default()?;
        let (verb, rest) = command
            .strip_prefix("/cron")
            .unwrap_or_default()
            .trim()
            .split_once(char::is_whitespace)
            .map(|(verb, rest)| (verb, rest.trim()))
            .unwrap_or_else(|| {
                let verb = command.strip_prefix("/cron").unwrap_or_default().trim();
                (verb, "")
            });
        match verb {
            "" | "list" => {
                let snapshot = store.snapshot(chrono::Utc::now())?;
                let mut lines = vec![format!(
                    "Cron {} · {}",
                    match snapshot.daemon {
                        CronDaemonStatus::Running => "running",
                        CronDaemonStatus::Stopped => "stopped",
                    },
                    snapshot.path.display()
                )];
                for job in snapshot.jobs {
                    let status = job
                        .state
                        .occurrences
                        .last()
                        .map(|run| format!("{:?}", run.status).to_ascii_lowercase())
                        .unwrap_or_else(|| "never run".into());
                    let next = job
                        .next_occurrences
                        .first()
                        .map(chrono::DateTime::to_rfc3339)
                        .unwrap_or_else(|| "none".into());
                    lines.push(format!(
                        "{}{} · {} · next {} · {}",
                        if job.job.enabled { "" } else { "[paused] " },
                        job.id,
                        job.description,
                        next,
                        status
                    ));
                }
                lines.push(
                    "Commands: /cron show ID | run ID | pause ID | resume ID | delete ID | upsert ID {JSON} | timezone IANA_ZONE"
                        .into(),
                );
                self.error = Some(lines.join("\n"));
            }
            "show" => {
                let snapshot = store.snapshot(chrono::Utc::now())?;
                let job = snapshot
                    .jobs
                    .into_iter()
                    .find(|job| job.id == rest)
                    .with_context(|| format!("cron job {rest:?} does not exist"))?;
                self.error = Some(serde_json::to_string_pretty(&job)?);
            }
            "run" => {
                store.run_now(rest, self.coordinator.clone()).await?;
                self.error = Some(format!("Cron job {rest:?} queued to run now."));
            }
            "pause" => {
                store.set_enabled(rest, false).await?;
                self.error = Some(format!("Cron job {rest:?} paused."));
            }
            "resume" => {
                store.set_enabled(rest, true).await?;
                self.error = Some(format!("Cron job {rest:?} resumed."));
            }
            "delete" => {
                if !store.delete(rest).await? {
                    anyhow::bail!("cron job {rest:?} does not exist");
                }
                self.error = Some(format!("Cron job {rest:?} deleted."));
            }
            "install" => {
                let view = store.install()?;
                self.error = Some(format!("Cron autostart: {:?}.", view.status));
            }
            "uninstall" => {
                let view = store.uninstall()?;
                self.error = Some(format!("Cron autostart: {:?}.", view.status));
            }
            "timezone" => {
                let mut document = store.load_or_create()?;
                document.timezone = rest.to_owned();
                store.save_document(&document).await?;
                self.error = Some(format!("Cron default timezone changed to {rest:?}."));
            }
            "upsert" => {
                let (id, json) = rest
                    .split_once(char::is_whitespace)
                    .context("usage: /cron upsert ID {JSON}")?;
                let job: CronJob =
                    serde_json::from_str(json.trim()).context("the cron job is not valid JSON")?;
                store.upsert(id, job).await?;
                self.error = Some(format!("Cron job {id:?} saved."));
            }
            _ => {
                self.error = Some(
                    "Usage: /cron [list|show ID|run ID|pause ID|resume ID|delete ID|upsert ID {JSON}|timezone IANA_ZONE|install|uninstall]"
                        .into(),
                );
            }
        }
        Ok(())
    }

    async fn command(&mut self, command: &str) -> Result<()> {
        match command {
            "/quit" => self.request_quit(),
            "/help" => self.show_help = true,
            "/models" => self.open_model_picker(),
            "/skills" => self.open_skill_picker(),
            "/sessions" => self.open_session_picker().await?,
            "/processes" => self.open_processes(),
            "/no-project" => self.create_no_project_session().await?,
            "/branches" => self.open_branch_navigator(),
            "/goals" => self.open_goal_picker(),
            "/usage" if self.usage_available() => {
                self.usage_open = true;
                self.usage_confirm = false;
                self.usage_reset_key = None;
                self.usage_scroll = 0;
                self.start_usage_refresh();
            }
            _ => self.error = Some(format!("Unknown command: {command}")),
        }
        Ok(())
    }

    async fn create_no_project_session(&mut self) -> Result<()> {
        if self.active_session {
            self.conversations
                .prepare_for_navigation(self.conversation.snapshot().session.id)
                .await?;
        }
        self.park_current_turn();
        let session = self.coordinator.create_no_project_session()?;
        let id = session.id;
        let mut agent = self.coordinator.build_agent(&self.runtime_root, session)?;
        let catalog = match agent.fetch_models().await {
            Ok(catalog) => {
                agent.resolve_new_session_model(&catalog);
                catalog
            }
            Err(error) => {
                self.error = Some(format!("Could not load the model catalog: {error:#}"));
                Vec::new()
            }
        };
        self.conversation = self.conversations.install(agent)?;
        self.conversation.persist().await?;
        self.model_catalogs.insert(id, catalog.clone());
        self.model_catalog = catalog;
        self.sync_active_session();
        self.error = None;
        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(());
        }
        if self.exit_confirm {
            match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    self.conversations.close_all_terminals()?;
                    self.exit_confirm = false;
                    self.should_quit = true;
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.exit_confirm = false,
                _ => {}
            }
            return Ok(());
        }
        if self.session_rename.is_some() {
            match key.code {
                KeyCode::Enter => self.finish_session_rename().await?,
                KeyCode::Esc => self.session_rename = None,
                KeyCode::Backspace => {
                    if let Some(rename) = self.session_rename.as_mut() {
                        rename.title.pop();
                    }
                }
                KeyCode::Char('u' | 'U')
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    if let Some(rename) = self.session_rename.as_mut() {
                        rename.title.clear();
                    }
                }
                KeyCode::Char(value)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        || key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    if let Some(rename) = self.session_rename.as_mut()
                        && rename.title.chars().count() < 120
                    {
                        rename.title.push(value);
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        if self.session_delete_confirm {
            match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    self.delete_session_selection().await?;
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.session_delete_confirm = false,
                _ => {}
            }
            return Ok(());
        }
        if self.process_dialog.is_some() {
            self.handle_process_key(key.code);
            return Ok(());
        }
        if self.usage_open {
            match key.code {
                KeyCode::Esc if self.usage_confirm => {
                    self.usage_confirm = false;
                    self.usage_reset_key = None;
                }
                KeyCode::Esc => {
                    self.usage_open = false;
                    self.usage_reset_key = None;
                    self.usage_scroll = 0;
                    self.usage_notice = None;
                }
                KeyCode::Char('r' | 'R') => self.start_usage_refresh(),
                KeyCode::Up => self.usage_scroll = self.usage_scroll.saturating_sub(1),
                KeyCode::Down => self.usage_scroll = self.usage_scroll.saturating_add(1),
                KeyCode::PageUp => self.usage_scroll = self.usage_scroll.saturating_sub(5),
                KeyCode::PageDown => self.usage_scroll = self.usage_scroll.saturating_add(5),
                KeyCode::Enter if self.usage_confirm && self.usage_state.can_reset => {
                    self.start_usage_reset()
                }
                KeyCode::Enter if self.usage_task.is_none() && self.usage_state.can_reset => {
                    self.usage_confirm = true;
                }
                _ => {}
            }
            return Ok(());
        }
        if self.branch_navigator.is_some() {
            match key.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                    self.move_branch_selection(key.code)?
                }
                KeyCode::Char('e' | 'E') => self.begin_selected_message_edit(),
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
        if self.queued_prompt_edit.is_some() && key.code == KeyCode::Esc {
            self.cancel_queued_prompt_edit();
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
        if self.provider_picker.is_some() {
            match key.code {
                KeyCode::Up => self.move_provider_selection(-1),
                KeyCode::Down => self.move_provider_selection(1),
                KeyCode::PageUp => self.move_provider_selection(-5),
                KeyCode::PageDown => self.move_provider_selection(5),
                KeyCode::Enter | KeyCode::Tab => self.accept_provider_selection().await?,
                KeyCode::Esc => self.provider_picker = None,
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
                KeyCode::Char('r' | 'R') => self.begin_session_rename(),
                KeyCode::Char('p' | 'P') => {
                    if let Some((_, _, _, session)) = self
                        .session_picker
                        .as_ref()
                        .and_then(SessionPicker::selected_session)
                    {
                        self.update_selected_session_metadata(
                            SessionMetadataUpdate::SetPinned(session.pinned_at.is_none()),
                            false,
                        )
                        .await?;
                    }
                }
                KeyCode::Char('a' | 'A') => {
                    if let Some((_, _, _, session)) = self
                        .session_picker
                        .as_ref()
                        .and_then(SessionPicker::selected_session)
                    {
                        self.update_selected_session_metadata(
                            SessionMetadataUpdate::SetArchived(session.archived_at.is_none()),
                            true,
                        )
                        .await?;
                    }
                }
                KeyCode::Delete | KeyCode::Backspace => self.delete_session_selection().await?,
                KeyCode::Esc => self.session_picker = None,
                _ => {}
            }
            return Ok(());
        }

        if self.editing_goal_id.is_some() && key.code == KeyCode::Esc {
            let id = self.editing_goal_id.take().expect("checked above");
            let resume = std::mem::take(&mut self.editing_goal_resume);
            self.clear_composer_text();
            self.preferred_column = None;
            self.close_completion();
            self.error = None;
            if resume && let Some(snapshot) = self.conversation.activate_goal(id).await? {
                self.apply_snapshot(snapshot);
                self.start_goal_continuation()?;
            }
            return Ok(());
        }
        if self.editing_message.is_some() && key.code == KeyCode::Esc {
            self.cancel_message_edit();
            return Ok(());
        }

        if is_dictation_shortcut(&key) {
            if let Err(error) = self.toggle_dictation() {
                self.error = Some(format!("Dictation failed: {error:#}"));
            }
            return Ok(());
        }

        if self.recording.is_some() {
            match recording_key_action(&key) {
                RecordingKeyAction::Cancel => {
                    self.recording = None;
                    self.send_after_transcription = false;
                    self.error = None;
                }
                RecordingKeyAction::StopAndSend => {
                    if let Err(error) = self.stop_dictation(true) {
                        self.error = Some(format!("Dictation failed: {error:#}"));
                    }
                }
                RecordingKeyAction::Ignore => {}
            }
            return Ok(());
        }

        if matches!(key.code, KeyCode::Char('v' | 'V'))
            && matches!(
                key.modifiers,
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER
            )
        {
            if let Err(error) = self.paste_operating_system_clipboard().await {
                self.error = Some(format!("Could not paste attachment: {error:#}"));
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
                    self.accept_completion().await;
                    return Ok(());
                }
                KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.accept_completion().await;
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
                KeyCode::Char('c' | 'd') if !self.is_busy() => self.request_quit(),
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
                if self.input.is_empty() {
                    self.recall_latest_user_message();
                } else {
                    self.move_vertical(-1);
                }
            }
            KeyCode::Down => {
                self.move_vertical(1);
            }
            KeyCode::PageUp => self.scroll_up(10),
            KeyCode::PageDown => self.scroll_down(10),
            KeyCode::F(1) => self.show_help = true,
            KeyCode::F(2) => self.open_skill_picker(),
            KeyCode::Esc => {
                self.clear_composer_text();
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
    runtime_root: PathBuf,
    debug_openai: DebugOutput,
    diagnostics: DiagnosticLog,
    config: Config,
    new_session: bool,
    coordinator: SessionCoordinator,
) -> Result<()> {
    let registry = coordinator.registry();
    let (model_catalog, catalog_error) = match agent.fetch_models().await {
        Ok(catalog) => {
            if new_session {
                agent.resolve_new_session_model(&catalog);
            } else {
                agent.resolve_auto_model(&catalog);
            }
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
            AppServices {
                debug_openai,
                usage_tracker: None,
                runtime_root,
            },
            config,
            registry,
            coordinator,
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

    if let Ok(Some(session_id)) = &result {
        println!("\x1b[2mSession saved as {session_id}\x1b[0m");
    }
    let report = diagnostics.report();
    if let Some(path) = report.path {
        println!(
            "CodeCrab wrote error diagnostics to {}.\n\
Use `--error-log <path>` to choose a different location.",
            path.display()
        );
    }
    if let Some(error) = report.failure {
        eprintln!("CodeCrab could not write its TUI error log: {error}");
    }
    result.map(|_| ())
}

async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut app: App,
) -> Result<Option<String>> {
    if app.active_goal_id().is_some() {
        app.start_goal_continuation()?;
    }
    let mut terminal_events = Vec::new();
    let mut last_spinner_tick = Instant::now();
    loop {
        app.drain_agent_events();
        app.drain_completion_updates();
        app.refresh_usage_for_finished_background_turns().await;
        app.finish_turn_if_ready().await?;
        app.finish_usage_task_if_ready().await?;
        if !app.is_running() {
            app.apply_pending_goal_action().await?;
            app.dispatch_queued_prompt_if_idle()?;
        }
        app.finish_transcription_if_ready().await?;
        app.refresh_process_dialog();
        app.update_drag_autoscroll();
        advance_spinner_if_due(&mut app.spinner, &mut last_spinner_tick, Instant::now());
        terminal.draw(|frame| render(frame, &mut app))?;

        if app.should_quit && !app.is_busy() {
            break;
        }
        collect_terminal_events(
            &mut terminal_events,
            TUI_TICK_RATE,
            event::poll,
            event::read,
        )?;
        for terminal_event in terminal_events.drain(..) {
            match terminal_event {
                Event::Key(key) => app.handle_key(key).await?,
                Event::Paste(text) => {
                    app.handle_paste(&text);
                }
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                _ => {}
            }
        }
    }

    let saved_session_id = app
        .active_session
        .then(|| app.conversation.snapshot().session)
        .filter(|session| !session.is_empty())
        .map(|session| session.id.to_string());
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
    Ok(saved_session_id)
}

fn advance_spinner_if_due(spinner: &mut usize, last_tick: &mut Instant, now: Instant) {
    if now.saturating_duration_since(*last_tick) >= TUI_TICK_RATE {
        *spinner = spinner.wrapping_add(1);
        *last_tick = now;
    }
}

fn collect_terminal_events(
    events: &mut Vec<Event>,
    timeout: Duration,
    mut poll: impl FnMut(Duration) -> io::Result<bool>,
    mut read: impl FnMut() -> io::Result<Event>,
) -> io::Result<()> {
    events.clear();
    if !poll(timeout)? {
        return Ok(());
    }
    events.push(read()?);
    while events.len() < MAX_TERMINAL_EVENTS_PER_FRAME && poll(Duration::ZERO)? {
        events.push(read()?);
    }
    Ok(())
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    app.composer_width = area.width.saturating_sub(2).max(1) as usize;
    let input_lines = composer_rows(&app.input, app.composer_width)
        .len()
        .clamp(1, 6) as u16;
    let goal_height = if app.visible_goal().is_some() { 3 } else { 0 };
    let fixed_height = 3 + 8 + goal_height + input_lines + 2;
    let available_queue_height = area.height.saturating_sub(fixed_height);
    let queued_height =
        ((app.prompt_queue.items.len() as u16 * 3).min(available_queue_height)) / 3 * 3;
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
    render_queued_prompts(frame, app, queued);
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
    if app.provider_picker.is_some() {
        render_provider_picker(frame, app, area);
    }
    if app.session_picker.is_some() {
        render_session_picker(frame, app, area);
    }
    if app.session_rename.is_some() {
        render_session_rename(frame, app, area);
    }
    if app.goal_picker.is_some() {
        render_goal_picker(frame, app, area);
    }
    if app.usage_open {
        render_usage(frame, app, area);
    }
    if app.process_dialog.is_some() {
        render_process_dialog(frame, app, area);
    }
    if app.session_delete_confirm {
        render_confirmation(
            frame,
            area,
            " Stop processes and delete session? ",
            "Every managed terminal in this session will be stopped before deletion.\n\nEnter/Y confirm  •  Esc/N cancel",
        );
    }
    if app.exit_confirm {
        render_confirmation(
            frame,
            area,
            " Stop processes and exit? ",
            "Every managed terminal will be stopped before CodeCrab exits.\n\nEnter/Y confirm  •  Esc/N cancel",
        );
    }
}

fn render_queued_prompts(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.queued_prompt_buttons.clear();
    let visible = (area.height / 3).min(app.prompt_queue.items.len() as u16) as usize;
    if visible == 0 || area.width < 16 {
        return;
    }
    let rows = Layout::vertical(vec![Constraint::Length(3); visible]).split(area);
    let total = app.prompt_queue.items.len();
    let editing_id = app.queued_prompt_edit.as_ref().map(|edit| edit.id);
    for (index, (prompt, row)) in app
        .prompt_queue
        .items
        .iter()
        .take(visible)
        .zip(rows.iter().copied())
        .enumerate()
    {
        let editing = editing_id == Some(prompt.id);
        if row.width < 25 {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        format!(
                            " {} {}/{}  ",
                            if editing { "EDITING" } else { "QUEUED" },
                            index + 1,
                            total
                        ),
                        Style::default().fg(if editing { CRAB } else { MUTED }),
                    ),
                    Span::raw(prompt.content.clone()),
                ]))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(if editing { CRAB } else { MUTED })),
                )
                .wrap(Wrap { trim: true }),
                row,
            );
            continue;
        }
        let compact_controls = row.width < 41;
        let (controls_width, steer_width, edit_width, delete_width) = if compact_controls {
            (9, 3, 3, 3)
        } else {
            (25, 9, 7, 9)
        };
        let [message, steer, edit, delete] = Layout::horizontal([
            Constraint::Length(row.width.saturating_sub(controls_width)),
            Constraint::Length(steer_width),
            Constraint::Length(edit_width),
            Constraint::Length(delete_width),
        ])
        .areas(row);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(
                        " {} {}/{}  ",
                        if editing { "EDITING" } else { "QUEUED" },
                        index + 1,
                        total
                    ),
                    Style::default().fg(if editing { CRAB } else { MUTED }),
                ),
                Span::raw(prompt.content.clone()),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if editing { CRAB } else { MUTED })),
            )
            .wrap(Wrap { trim: true }),
            message,
        );
        frame.render_widget(
            Paragraph::new(if compact_controls { "S" } else { "Steer" })
                .centered()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(if editing { MUTED } else { CRAB })),
                ),
            steer,
        );
        frame.render_widget(
            Paragraph::new(if compact_controls { "E" } else { "Edit" })
                .centered()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(CRAB)),
                ),
            edit,
        );
        frame.render_widget(
            Paragraph::new(if compact_controls { "X" } else { "Delete" })
                .centered()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(MUTED)),
                ),
            delete,
        );
        app.queued_prompt_buttons.push(QueuedPromptButtons {
            id: prompt.id,
            steer,
            edit,
            delete,
        });
    }
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
    let usage = app.usage_indicator();
    let provider = (app.config.providers.len() > 1).then(|| app.session_provider());
    let separator = "  │  ";
    let model_section_width = provider
        .as_ref()
        .map_or(0, |provider| provider.chars().count() + 1)
        + app.model.chars().count()
        + thinking.chars().count()
        + 1
        + if fast { 2 } else { 0 };
    let fixed_width = status.chars().count()
        + model_section_width
        + usage.as_ref().map_or(0, |usage| usage.chars().count())
        + separator.chars().count() * if usage.is_some() { 3 } else { 2 };
    let mut spans = vec![
        Span::styled(status, Style::default().fg(status_color)),
        Span::styled(separator, Style::default().fg(MUTED)),
    ];
    if let Some(provider) = provider {
        spans.extend([
            Span::styled(provider, Style::default().fg(MUTED)),
            Span::raw(" "),
        ]);
    }
    spans.push(Span::styled(&app.model, Style::default().fg(Color::White)));
    if fast {
        spans.push(Span::styled(" ⚡", Style::default().fg(Color::Yellow)));
    }
    spans.extend([
        Span::raw(" "),
        Span::styled(thinking, Style::default().fg(AQUA)),
    ]);
    if let Some(usage) = usage {
        spans.extend([
            Span::styled(separator, Style::default().fg(MUTED)),
            Span::styled(usage, Style::default().fg(AQUA)),
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
    let inner = area.inner(Margin {
        horizontal: 0,
        vertical: 1,
    });

    let source = conversation_source(app);
    let rows = wrap_conversation_lines(
        &source.lines,
        &source.user_lines,
        &source.single_row_lines,
        inner.width.max(1),
    );
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
    if let Some(activity_id) = app.pending_activity_id.take()
        && let Some(source_line) = source
            .activity_lines
            .iter()
            .find_map(|(id, line)| (id == &activity_id).then_some(*line))
        && let Some(activity_row) = rows.iter().position(|row| row.source_line == source_line)
    {
        app.scroll = activity_row.saturating_sub(2).min(u16::MAX as usize) as u16;
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
    let visible_rows = rows
        .iter()
        .enumerate()
        .skip(app.scroll as usize)
        .take(inner.height as usize)
        .collect::<Vec<_>>();
    for (viewport_row, (_, row)) in visible_rows.iter().enumerate() {
        if row.user_message {
            frame.render_widget(
                Block::default().style(Style::default().bg(USER_MESSAGE_BG)),
                Rect::new(inner.x, inner.y + viewport_row as u16, inner.width, 1),
            );
        }
    }
    let visible = visible_rows
        .into_iter()
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
        user_lines: HashSet::new(),
        single_row_lines: HashSet::new(),
        copy_targets: Vec::new(),
        turn_toggles: Vec::new(),
        node_lines: Vec::new(),
        activity_lines: Vec::new(),
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
            push_user_message(&mut source, content);
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
        push_user_message(&mut source, content);
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
        ancestor_continuations: Vec<Vec<Uuid>>,
        following_siblings: Vec<Uuid>,
        children: &HashMap<Option<Uuid>, Vec<Uuid>>,
        rows: &mut Vec<BranchRow>,
    ) {
        rows.push(BranchRow {
            id,
            depth,
            ancestor_continuations: ancestor_continuations.clone(),
            following_siblings: following_siblings.clone(),
        });
        let descendants = children.get(&Some(id)).map(Vec::as_slice).unwrap_or(&[]);
        for (index, child) in descendants.iter().enumerate() {
            let mut continuations = ancestor_continuations.clone();
            if depth > 0 {
                continuations.push(following_siblings.clone());
            }
            visit(
                *child,
                depth + 1,
                continuations,
                descendants[index + 1..].to_vec(),
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
            roots[index + 1..].to_vec(),
            &children,
            &mut rows,
        );
    }
    rows
}

fn branch_segment_style(previewed: bool, original: bool, selected: bool) -> Style {
    let mut style = Style::default().fg(if previewed {
        CRAB
    } else if original {
        AQUA
    } else {
        MUTED
    });
    if previewed {
        style = style.add_modifier(Modifier::BOLD);
    }
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn branch_row_line(
    row: &BranchRow,
    selected_id: Uuid,
    preview_path: &HashSet<Uuid>,
    original_path: &HashSet<Uuid>,
) -> Line<'static> {
    let selected = row.id == selected_id;
    let row_previewed = preview_path.contains(&row.id);
    let row_original = original_path.contains(&row.id);
    let row_style = branch_segment_style(row_previewed, row_original, selected);
    let mut spans = Vec::new();
    for targets in &row.ancestor_continuations {
        if !targets.is_empty() {
            let previewed = targets.iter().any(|id| preview_path.contains(id));
            let original = targets.iter().any(|id| original_path.contains(id));
            spans.push(Span::styled(
                if previewed { "┃ " } else { "│ " },
                branch_segment_style(previewed, original, selected),
            ));
        } else {
            spans.push(Span::styled("  ", row_style));
        }
    }
    if row.depth > 0 {
        let following_previewed = row
            .following_siblings
            .iter()
            .any(|id| preview_path.contains(id));
        let following_original = row
            .following_siblings
            .iter()
            .any(|id| original_path.contains(id));
        let junction_previewed = row_previewed || following_previewed;
        let junction_original = row_original || following_original;
        let junction = if row.following_siblings.is_empty() {
            if row_previewed { "┗" } else { "└" }
        } else if junction_previewed {
            "┣"
        } else {
            "├"
        };
        spans.push(Span::styled(
            junction,
            branch_segment_style(junction_previewed, junction_original, selected),
        ));
        spans.push(Span::styled(
            if row_previewed { "━" } else { "─" },
            row_style,
        ));
    }
    spans.push(Span::styled(if selected { "◉" } else { "●" }, row_style));
    Line::from(spans)
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
        lines.push(branch_row_line(
            row,
            navigator.nodes[navigator.selected].id,
            &navigator.preview_path,
            &navigator.original_path,
        ));
    }
    while lines.len() + 1 < area.height as usize {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        " arrows · e edit · ↵ · esc",
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

fn attachment_bindings_for_message(message: &Message) -> Vec<AttachmentBinding> {
    let mut cursor = 0;
    let mut bindings = Vec::new();
    for part in &message.parts {
        match part {
            MessagePart::Text { text } => cursor += text.len(),
            MessagePart::Attachment {
                attachment_id,
                reference,
                ..
            } => {
                bindings.push(AttachmentBinding {
                    attachment_id: *attachment_id,
                    start: cursor,
                    end: cursor + reference.len(),
                });
                cursor += reference.len();
            }
        }
    }
    bindings
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

fn push_user_message(source: &mut ConversationSource, content: &str) {
    let first_line = source.lines.len();
    source.lines.push(Line::from(Span::styled(
        " USER ",
        Style::default()
            .fg(AQUA)
            .bg(USER_MESSAGE_BG)
            .add_modifier(Modifier::BOLD),
    )));
    push_message_lines(source, content, true, None);
    source.user_lines.extend(first_line..source.lines.len());
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
    source
        .activity_lines
        .push((activity.id.clone(), line_index));
    source.lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(icon, Style::default().fg(icon_color)),
        Span::styled(
            title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(detail.clone(), Style::default().fg(MUTED)),
    ]));
    if matches!(activity.tool.as_str(), "shell" | "shell_noninteractive") {
        source.single_row_lines.insert(line_index);
    }
    if matches!(
        activity.tool.as_str(),
        "shell" | "shell_noninteractive" | "read_file" | "write_file" | "replace_in_file"
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

fn wrap_conversation_lines(
    lines: &[Line<'static>],
    user_lines: &HashSet<usize>,
    single_row_lines: &HashSet<usize>,
    width: u16,
) -> Vec<VisualRow> {
    let mut rows = Vec::new();
    for (source_line, line) in lines.iter().enumerate() {
        let mut row = VisualRow {
            source_line,
            units: Vec::new(),
            user_message: user_lines.contains(&source_line),
        };
        let mut row_width = 0;
        let mut source_offset = 0;
        let source_length = line
            .spans
            .iter()
            .map(|span| span.content.len())
            .sum::<usize>();
        'content: for span in &line.spans {
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
                    if single_row_lines.contains(&source_line) {
                        let mut omitted_start = source_start;
                        while !row.units.is_empty() && row_width.saturating_add(1) > width {
                            let removed = row.units.pop().expect("row was checked as non-empty");
                            row_width = row_width.saturating_sub(removed.width);
                            omitted_start = removed.source_start;
                        }
                        row.units.push(VisualUnit {
                            text: "…".into(),
                            width: 1,
                            style: span.style,
                            source_line,
                            source_start: omitted_start,
                            source_end: source_length,
                        });
                        break 'content;
                    }
                    rows.push(std::mem::take(&mut row));
                    row.source_line = source_line;
                    row.user_message = user_lines.contains(&source_line);
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
    } else if app.editing_message.is_some() {
        " Edit message · Enter branch · Esc cancel "
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
        && app.provider_picker.is_none()
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
    let popup = centered_rect(area, 74, 25);
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
        Line::from("  Ctrl/Alt/Command+V    paste clipboard files or image"),
        Line::from("  Esc                   discard active recording"),
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
        Line::from("  /branches  browse conversation branches"),
        Line::from("  /cron      manage scheduled agent tasks"),
        Line::from("  /goal ...  start a persistent goal"),
        Line::from("  /goals     manage persistent goals"),
        Line::from("  /no-project create a session without a project"),
        Line::from("  /models    choose model, thinking, and speed"),
        Line::from("  /processes manage running shell terminals"),
        Line::from("  /providers choose the current session provider"),
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

fn render_provider_picker(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(picker) = &app.provider_picker else {
        return;
    };
    let notice_height = picker
        .notice
        .as_deref()
        .map_or(0, |notice| notice.lines().count() as u16 + 1);
    let height = (picker.providers.len().min(12) as u16 + 5 + notice_height)
        .clamp(8, area.height.saturating_sub(2).max(8));
    let popup = centered_rect(area, 82, height);
    frame.render_widget(Clear, popup);
    let mut lines = vec![
        Line::from(Span::styled(
            "↑↓ select  •  Enter/Tab choose  •  Esc close",
            Style::default().fg(MUTED),
        )),
        Line::default(),
    ];
    for (index, name) in picker.providers.iter().enumerate() {
        let selected = index == picker.selected;
        let detail = app
            .config
            .providers
            .get(name)
            .map(|provider| format!("{} · {}", provider.model, provider.base_url))
            .unwrap_or_default();
        lines.push(
            Line::from(vec![
                Span::styled(
                    if selected { " › " } else { "   " },
                    Style::default().fg(CRAB),
                ),
                Span::styled(
                    format!("{name:<18}"),
                    Style::default()
                        .fg(if selected { Color::White } else { AQUA })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(compact_text(&detail, 54), Style::default().fg(MUTED)),
            ])
            .style(if selected {
                Style::default().bg(Color::Rgb(42, 48, 58))
            } else {
                Style::default()
            }),
        );
    }
    if let Some(notice) = &picker.notice {
        lines.push(Line::default());
        lines.extend(notice.lines().map(|line| {
            Line::from(Span::styled(
                line.to_owned(),
                Style::default().fg(Color::Yellow),
            ))
        }));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(CRAB))
                .title(Span::styled(
                    " Providers ",
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

fn render_process_dialog(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(dialog) = app.process_dialog.as_mut() else {
        return;
    };
    match &dialog.view {
        ProcessDialogView::List => {
            let height = (dialog.processes.len().min(12) as u16 + 6)
                .clamp(9, area.height.saturating_sub(2).max(9));
            let popup = centered_rect(area, 88, height);
            frame.render_widget(Clear, popup);
            let mut lines = vec![
                Line::from(Span::styled(
                    "↑↓ select  •  Enter/O output  •  G origin  •  K stop  •  Esc close",
                    Style::default().fg(MUTED),
                )),
                Line::default(),
            ];
            if dialog.processes.is_empty() {
                lines.push(Line::from(Span::styled(
                    "No managed terminals are running in this session.",
                    Style::default().fg(MUTED),
                )));
            }
            for (index, process) in dialog.processes.iter().enumerate() {
                let selected = index == dialog.selected;
                let duration = format_live_duration(process.created_at);
                lines.push(
                    Line::from(vec![
                        Span::styled(
                            if selected { " › " } else { "   " },
                            Style::default().fg(Color::Magenta),
                        ),
                        Span::styled(format!("{:<10}", duration), Style::default().fg(AQUA)),
                        Span::styled(
                            compact_text(&process.command.replace('\n', " "), 64),
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
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Magenta))
                        .title(Span::styled(
                            format!(" Processes · {} running ", dialog.processes.len()),
                            Style::default()
                                .fg(Color::Magenta)
                                .add_modifier(Modifier::BOLD),
                        )),
                ),
                popup,
            );
        }
        ProcessDialogView::Output(_) => {
            let popup = centered_rect(area, 94, 88);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Magenta))
                .title(Span::styled(
                    " Process output ",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);
            let [header, output_area, footer] = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .areas(inner);
            if let Some(output) = &dialog.output {
                let (status, status_color) = match output.process_state {
                    TerminalProcessState::Running => ("RUNNING", Color::Yellow),
                    TerminalProcessState::Exited => ("EXITED", Color::Green),
                    TerminalProcessState::Closed => ("STOPPED", Color::Red),
                    TerminalProcessState::Interrupted => ("INTERRUPTED", Color::Red),
                };
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(vec![
                            Span::styled(
                                format!(" {status} "),
                                Style::default()
                                    .fg(status_color)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!("{}  ", format_live_duration(output.created_at)),
                                Style::default().fg(AQUA),
                            ),
                            Span::styled(
                                compact_text(&output.command.replace('\n', " "), 72),
                                Style::default().fg(Color::White),
                            ),
                        ]),
                        Line::default(),
                    ]),
                    header,
                );
                let lines = output
                    .lines
                    .iter()
                    .map(|line| {
                        Line::from(
                            line.spans
                                .iter()
                                .map(|span| {
                                    Span::styled(
                                        span.text.clone(),
                                        terminal_ratatui_style(&span.style),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>();
                let maximum_scroll = lines
                    .len()
                    .saturating_sub(output_area.height as usize)
                    .min(u16::MAX as usize) as u16;
                if dialog.output_follow {
                    dialog.output_scroll = maximum_scroll;
                } else {
                    dialog.output_scroll = dialog.output_scroll.min(maximum_scroll);
                }
                frame.render_widget(
                    Paragraph::new(lines).scroll((dialog.output_scroll, 0)),
                    output_area,
                );
            } else {
                frame.render_widget(Paragraph::new("Waiting for terminal output…"), output_area);
            }
            frame.render_widget(
                Paragraph::new("↑↓/Pg scroll  •  End follow  •  G origin  •  K stop  •  Esc list")
                    .style(Style::default().fg(MUTED)),
                footer,
            );
        }
    }
    if dialog.stop_confirm {
        render_confirmation(
            frame,
            area,
            " Stop this process? ",
            "The complete managed process tree will be terminated. The agent turn will continue.\n\nEnter/Y confirm  •  Esc/N cancel",
        );
    }
}

fn format_live_duration(created_at: chrono::DateTime<chrono::Utc>) -> String {
    let seconds = chrono::Utc::now()
        .signed_duration_since(created_at)
        .num_seconds()
        .max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

fn terminal_ratatui_style(style: &TerminalStyle) -> Style {
    let mut foreground = parse_hex_color(&style.foreground).unwrap_or(Color::White);
    let mut background = parse_hex_color(&style.background).unwrap_or(Color::Black);
    if style.reverse {
        std::mem::swap(&mut foreground, &mut background);
    }
    let mut rendered = Style::default().fg(foreground).bg(background);
    if style.bold {
        rendered = rendered.add_modifier(Modifier::BOLD);
    }
    if style.faint {
        rendered = rendered.add_modifier(Modifier::DIM);
    }
    if style.italic {
        rendered = rendered.add_modifier(Modifier::ITALIC);
    }
    if style.underline != "none" {
        rendered = rendered.add_modifier(Modifier::UNDERLINED);
    }
    if style.strikethrough {
        rendered = rendered.add_modifier(Modifier::CROSSED_OUT);
    }
    rendered
}

fn parse_hex_color(value: &str) -> Option<Color> {
    let value = value.strip_prefix('#')?;
    (value.len() == 6).then_some(Color::Rgb(
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
    ))
}

fn render_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    message: &'static str,
) {
    let popup = centered_rect(area, 66, 9);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(message)
            .centered()
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red))
                    .title(Span::styled(
                        title,
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
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
    let current_id = app
        .active_session
        .then(|| app.conversation.snapshot().session.id);
    let mut lines = vec![
        Line::from(Span::styled(
            "↑↓ select  •  Enter resume  •  R rename  •  P pin  •  A archive  •  Del delete",
            Style::default().fg(MUTED),
        )),
        Line::default(),
    ];
    for (index, row) in rows.iter().enumerate().skip(start).take(available) {
        let selected = index == picker.selected;
        let line = match *row {
            SessionPickerRow::Project(project_index) => {
                let project = &picker.projects[project_index];
                let active = match (&project.project.root, app.session_scope) {
                    (None, SessionScope::NoProject) => true,
                    (Some(root), SessionScope::Project) => paths_equal(root, &app.project_root),
                    _ => false,
                };
                let project_label = project
                    .project
                    .root
                    .as_ref()
                    .map(|root| root.display().to_string())
                    .unwrap_or_else(|| "No project".into());
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
                        compact_path(&project_label, 54),
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
            SessionPickerRow::Section(project_index, section) => {
                let project = &picker.projects[project_index];
                let (icon, title, count) = match section {
                    SessionSection::Pinned => ("󰐃", "Pinned", project.pinned_sessions.len()),
                    SessionSection::Active => ("", "Sessions", project.active_sessions.len()),
                    SessionSection::Archived => (
                        if project.archived_expanded {
                            "▾"
                        } else {
                            "▸"
                        },
                        "Archived",
                        project
                            .archived_sessions
                            .iter()
                            .filter(|session| session.depth == 0)
                            .count(),
                    ),
                };
                Line::from(vec![
                    Span::styled(
                        if selected { " › " } else { "   " },
                        Style::default().fg(CRAB),
                    ),
                    Span::styled(
                        format!("  {icon} {title}  {count}"),
                        Style::default()
                            .fg(if selected { Color::White } else { MUTED })
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
            }
            SessionPickerRow::Session(project_index, section, session_index) => {
                let session = &picker.projects[project_index].sessions(section)[session_index];
                let collapsed = section != SessionSection::Pinned
                    && picker.collapsed_sessions.contains(&session.id);
                let active_group = match (
                    &picker.projects[project_index].project.root,
                    app.session_scope,
                ) {
                    (None, SessionScope::NoProject) => true,
                    (Some(root), SessionScope::Project) => paths_equal(root, &app.project_root),
                    _ => false,
                };
                let active = Some(session.id) == current_id && active_group;
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
                let active_terminal_count = app
                    .conversations
                    .get(session.id)
                    .map(|conversation| conversation.running_terminal_count())
                    .unwrap_or(0);
                let session_title = if session.scheduled_run.is_some() {
                    format!("[cron] {}", session.title)
                } else {
                    session.title.clone()
                };
                Line::from(vec![
                    Span::styled(
                        if selected { " › " } else { "   " },
                        Style::default().fg(AQUA),
                    ),
                    Span::raw("  ".repeat(session.depth)),
                    Span::styled(
                        if session.descendant_count == 0 {
                            "  "
                        } else if collapsed {
                            "▸ "
                        } else {
                            "▾ "
                        },
                        Style::default().fg(CRAB),
                    ),
                    Span::styled(if active { "● " } else { "  " }, Style::default().fg(AQUA)),
                    Span::styled(
                        if session.pinned_at.is_some() {
                            "󰐃 "
                        } else {
                            "  "
                        },
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        if session.archived_at.is_some() || session.archived_by_ancestor {
                            "󰀼 "
                        } else {
                            "  "
                        },
                        Style::default().fg(if session.archived_at.is_some() {
                            Color::Yellow
                        } else {
                            MUTED
                        }),
                    ),
                    Span::styled(
                        format!("{:<20}", compact_text(&session_title, 19)),
                        Style::default()
                            .fg(if selected { Color::White } else { AQUA })
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if active_terminal_count > 0 {
                            format!(" 󰆍 {active_terminal_count} ")
                        } else {
                            String::new()
                        },
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if collapsed {
                            format!(" +{} ", session.descendant_count)
                        } else if session.depth == 0 && !session.ancestor_titles.is_empty() {
                            format!(
                                " via {} ",
                                compact_text(
                                    &session
                                        .ancestor_titles
                                        .iter()
                                        .rev()
                                        .take(2)
                                        .rev()
                                        .cloned()
                                        .collect::<Vec<_>>()
                                        .join(" › "),
                                    18,
                                )
                            )
                        } else {
                            String::new()
                        },
                        Style::default().fg(CRAB),
                    ),
                    Span::styled(
                        format!(
                            " {:<10}  {:<8}  {}  C {}  U {}",
                            compact_text(&session.model, 9),
                            status,
                            &session.id.to_string()[..8],
                            session.created_at.format("%m-%d %H:%M"),
                            session.updated_at.format("%m-%d %H:%M")
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

fn render_session_rename(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(rename) = &app.session_rename else {
        return;
    };
    let popup = centered_rect(area, 66, 7);
    frame.render_widget(Clear, popup);
    let text = format!("{}█", rename.title);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(text),
            Line::default(),
            Line::from(Span::styled(
                "Enter save  •  Esc cancel  •  Ctrl+U clear",
                Style::default().fg(MUTED),
            )),
        ])
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(CRAB))
                .title(Span::styled(
                    " Rename session ",
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

fn render_usage(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut lines = vec![Line::from(Span::styled(
        "OpenAI ChatGPT plan usage",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ))];
    if let Some(snapshot) = &app.usage_state.snapshot {
        lines.push(Line::from(Span::styled(
            format!("Plan: {}", snapshot.plan_type),
            Style::default().fg(MUTED),
        )));
        lines.push(Line::default());
        for window in &snapshot.windows {
            let name = window.limit_name.clone().unwrap_or_else(|| "Codex".into());
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{name}: "),
                    Style::default().fg(AQUA).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "{}% used / {}% remaining",
                    window.used_percent, window.remaining_percent
                )),
            ]));
            lines.push(Line::from(Span::styled(
                format!(
                    "  {} window · Resets {}",
                    format_usage_duration(window.window_duration_seconds),
                    format_local_reset(window.resets_at, "%a, %b %-d at %H:%M %Z")
                ),
                Style::default().fg(MUTED),
            )));
        }
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("Manual resets: ", Style::default().fg(AQUA)),
            Span::raw(format!(
                "{} available",
                snapshot.reset_credits.available_count
            )),
        ]));
        for credit in &snapshot.reset_credits.credits {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}{}",
                    credit.title.as_deref().unwrap_or("Reset credit"),
                    credit
                        .expires_at
                        .as_ref()
                        .map(|value| format!(" · Expires {}", format_credit_expiry(value)))
                        .unwrap_or_default()
                ),
                Style::default().fg(MUTED),
            )));
            if let Some(description) = &credit.description {
                lines.push(Line::from(Span::styled(
                    format!("    {description}"),
                    Style::default().fg(MUTED),
                )));
            }
        }
    } else {
        lines.extend([Line::default(), Line::from("Usage unavailable")]);
    }
    if app.usage_state.stale {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            app.usage_state
                .error
                .as_deref()
                .unwrap_or("Usage unavailable"),
            Style::default().fg(Color::Yellow),
        )));
        if let Some(updated) = app.usage_state.last_updated_at {
            lines.push(Line::from(Span::styled(
                format!(
                    "Last updated {}",
                    format_local_reset(updated, "%a, %b %-d at %H:%M:%S %Z")
                ),
                Style::default().fg(MUTED),
            )));
        }
    }
    if let Some(notice) = &app.usage_notice {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            notice.clone(),
            Style::default().fg(AQUA),
        )));
    }
    lines.push(Line::default());
    if app.usage_task.is_some() {
        lines.push(Line::from(Span::styled(
            "Refreshing usage…",
            Style::default().fg(Color::Yellow),
        )));
    } else if app.usage_confirm {
        if app.usage_state.can_reset {
            let remaining = app
                .usage_state
                .snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.reset_credits.available_count - 1);
            lines.push(Line::from(Span::styled(
                format!(
                    "Reset usage now? This consumes 1 credit ({remaining} remaining). This cannot be undone."
                ),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                "Enter confirm · Esc cancel",
                Style::default().fg(MUTED),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "Reset confirmation paused because current usage cannot be reset. Refresh to verify the account state, or cancel.",
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(Span::styled(
                "R refresh · Esc cancel",
                Style::default().fg(MUTED),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            if app.usage_state.can_reset {
                "↑↓ scroll · R refresh · Enter reset usage · Esc close"
            } else {
                "↑↓ scroll · R retry/refresh · Esc close"
            },
            Style::default().fg(MUTED),
        )));
    }
    let height = (lines.len() as u16 + 2).clamp(10, area.height.saturating_sub(2).max(10));
    let popup = centered_rect(area, 82, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.usage_scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(CRAB))
                    .title(Span::styled(
                        " Usage ",
                        Style::default().fg(CRAB).add_modifier(Modifier::BOLD),
                    )),
            ),
        popup,
    );
}

fn format_usage_duration(seconds: i64) -> String {
    if seconds > 0 && seconds % 604_800 == 0 {
        let weeks = seconds / 604_800;
        format!("{weeks} week{}", if weeks == 1 { "" } else { "s" })
    } else if seconds > 0 && seconds % 86_400 == 0 {
        let days = seconds / 86_400;
        format!("{days} day{}", if days == 1 { "" } else { "s" })
    } else if seconds > 0 && seconds % 3_600 == 0 {
        let hours = seconds / 3_600;
        format!("{hours} hour{}", if hours == 1 { "" } else { "s" })
    } else {
        format!("{} minutes", seconds.max(0) / 60)
    }
}

fn format_credit_expiry(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value).map_or_else(
        |_| value.to_owned(),
        |date| {
            date.with_timezone(&Local)
                .format("%a, %b %-d at %H:%M %Z")
                .to_string()
        },
    )
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

fn format_local_reset(timestamp: i64, format: &str) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|value| value.format(format).to_string())
        .unwrap_or_else(|| "unknown".into())
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

#[derive(Debug, PartialEq, Eq)]
enum RecordingKeyAction {
    Cancel,
    StopAndSend,
    Ignore,
}

fn recording_key_action(key: &KeyEvent) -> RecordingKeyAction {
    if key.code == KeyCode::Esc {
        RecordingKeyAction::Cancel
    } else if key.code == KeyCode::Enter
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT)
    {
        RecordingKeyAction::StopAndSend
    } else {
        RecordingKeyAction::Ignore
    }
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
        println!(
            "\n{}",
            project
                .root
                .as_ref()
                .map(|root| root.display().to_string())
                .unwrap_or_else(|| "No project".into())
        );
        println!(
            "  {:<10}  {:<10}  {:<6}  {:<8}  {:<20}  {:<20}  {:<18}  TITLE",
            "ID", "PARENT", "PINNED", "ARCHIVED", "CREATED", "UPDATED", "MODEL"
        );
        for session in &project.sessions {
            let parent = session
                .parent_session_id
                .map(|id| id.to_string()[..8].to_owned())
                .unwrap_or_else(|| "-".into());
            println!(
                "  {:<10}  {:<10}  {:<6}  {:<8}  {:<20}  {:<20}  {:<18}  {}{}",
                &session.id.to_string()[..8],
                parent,
                if session.pinned_at.is_some() {
                    "yes"
                } else {
                    "-"
                },
                if session.archived_at.is_some() {
                    "direct"
                } else if session.archived_by_ancestor {
                    "inherited"
                } else {
                    "-"
                },
                session.created_at.format("%Y-%m-%d %H:%M"),
                session.updated_at.format("%Y-%m-%d %H:%M"),
                session.model,
                "  ".repeat(session.depth),
                if session.scheduled_run.is_some() {
                    format!("[cron] {}", session.title)
                } else {
                    session.title.clone()
                }
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        fs,
        path::{Path, PathBuf},
    };

    use super::*;
    use ratatui::backend::TestBackend;

    use crate::{
        account_usage::{ResetCredit, ResetCredits, UsageSnapshot, UsageWindow, UsageWindowKind},
        completion::{NERD_FOLDER, file_completion_context, file_icon},
        config::{Config, ModelCapabilitiesConfig, ProviderConfig, SessionRegistry, paths_equal},
        provider::{
            FunctionCall, ModelCatalogEntry, OpenAiCompatible, ReasoningOption, ServiceTierOption,
            ToolCall,
        },
        skills::SkillRegistry,
        tools::ToolBox,
    };

    #[test]
    fn issue_87_terminal_event_burst_is_drained_before_the_next_render() {
        let pending = RefCell::new(
            (0..64)
                .map(|_| {
                    Event::Mouse(MouseEvent {
                        kind: MouseEventKind::ScrollUp,
                        column: 40,
                        row: 12,
                        modifiers: KeyModifiers::NONE,
                    })
                })
                .collect::<VecDeque<_>>(),
        );

        let mut events = Vec::new();
        collect_terminal_events(
            &mut events,
            Duration::ZERO,
            |_| Ok(!pending.borrow().is_empty()),
            || {
                pending.borrow_mut().pop_front().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "event queue empty")
                })
            },
        )
        .unwrap();

        assert_eq!(
            events.len(),
            64,
            "a wheel burst must be drained before another full conversation render"
        );
        assert!(
            pending.borrow().is_empty(),
            "queued wheel events would otherwise replay across later frames"
        );
    }

    #[test]
    fn terminal_event_batches_wait_once_then_poll_without_blocking() {
        let pending = RefCell::new(VecDeque::from([
            Event::FocusGained,
            Event::FocusLost,
            Event::Resize(100, 30),
        ]));
        let timeouts = RefCell::new(Vec::new());
        let mut events = Vec::new();

        collect_terminal_events(
            &mut events,
            Duration::from_millis(70),
            |timeout| {
                timeouts.borrow_mut().push(timeout);
                Ok(!pending.borrow().is_empty())
            },
            || {
                pending.borrow_mut().pop_front().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "event queue empty")
                })
            },
        )
        .unwrap();

        assert_eq!(events.len(), 3);
        assert_eq!(
            *timeouts.borrow(),
            [
                Duration::from_millis(70),
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            ]
        );
    }

    #[test]
    fn terminal_event_batches_are_bounded_to_keep_rendering_live() {
        let pending = RefCell::new(VecDeque::from(vec![
            Event::FocusGained;
            MAX_TERMINAL_EVENTS_PER_FRAME + 5
        ]));
        let mut events = Vec::new();

        collect_terminal_events(
            &mut events,
            Duration::ZERO,
            |_| Ok(!pending.borrow().is_empty()),
            || {
                pending.borrow_mut().pop_front().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "event queue empty")
                })
            },
        )
        .unwrap();

        assert_eq!(events.len(), MAX_TERMINAL_EVENTS_PER_FRAME);
        assert_eq!(pending.borrow().len(), 5);
    }

    #[test]
    fn spinner_speed_depends_on_elapsed_time_not_event_loop_iterations() {
        let start = Instant::now();
        let mut last_tick = start;
        let mut spinner = 0;

        for _ in 0..1_000 {
            advance_spinner_if_due(
                &mut spinner,
                &mut last_tick,
                start + TUI_TICK_RATE - Duration::from_millis(1),
            );
        }
        assert_eq!(spinner, 0);

        advance_spinner_if_due(&mut spinner, &mut last_tick, start + TUI_TICK_RATE);
        assert_eq!(spinner, 1);

        for _ in 0..1_000 {
            advance_spinner_if_due(&mut spinner, &mut last_tick, start + TUI_TICK_RATE);
        }
        assert_eq!(spinner, 1);

        advance_spinner_if_due(
            &mut spinner,
            &mut last_tick,
            start + TUI_TICK_RATE + TUI_TICK_RATE,
        );
        assert_eq!(spinner, 2);
    }

    fn test_registry(root: &Path) -> SessionRegistry {
        SessionRegistry::at(root.join(".test-global-config").join("config.toml"))
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
        let registry = test_registry(root);
        let coordinator = SessionCoordinator::new(
            config.clone(),
            registry.clone(),
            DebugOutput::default(),
            DiagnosticLog::default(),
            root.to_path_buf(),
            root.join(".test-global-config").join("AGENTS.md"),
        );
        let provider = OpenAiCompatible::new(&config, &config.active_provider).unwrap();
        App::new(
            Agent::new(
                provider,
                ToolBox::new(root.to_path_buf()),
                SkillRegistry::default(),
                session,
                root.join(".test-global-config").join("AGENTS.md"),
                DiagnosticLog::default(),
            )
            .unwrap(),
            Vec::new(),
            None,
            AppServices {
                debug_openai: DebugOutput::default(),
                usage_tracker: Some(UsageTracker::test(false, None).unwrap()),
                runtime_root: root.to_path_buf(),
            },
            config,
            registry,
            coordinator,
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

    fn interleaved_branching_test_app(root: &Path) -> (App, Uuid, Uuid, Uuid) {
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
        session
            .messages
            .push(Message::text(Role::User, "root request"));
        let root_answer = session
            .messages
            .push(Message::text(Role::Assistant, "root answer"));
        session
            .messages
            .push(Message::text(Role::User, "first branch"));
        let first_answer = session
            .messages
            .push(Message::text(Role::Assistant, "first answer"));
        session
            .messages
            .push(Message::text(Role::User, "deep branch"));
        session
            .messages
            .push(Message::text(Role::Assistant, "deep answer"));
        let deep_child = session
            .messages
            .push(Message::text(Role::User, "deep child"));
        let second_branch = session
            .messages
            .branch_from(
                Some(root_answer),
                Message::text(Role::User, "second branch"),
            )
            .unwrap();
        let later_first_branch = session
            .messages
            .branch_from(
                Some(first_answer),
                Message::text(Role::User, "later first branch"),
            )
            .unwrap();
        (
            test_app_with_session(root, config, session),
            deep_child,
            later_first_branch,
            second_branch,
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

    #[test]
    fn branch_navigator_vertical_movement_follows_the_rendered_row_order() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, deep_child, later_first_branch, second_branch) =
            interleaved_branching_test_app(root.path());
        app.open_branch_navigator();
        let navigator = app.branch_navigator.as_ref().unwrap();
        assert_eq!(navigator.nodes[navigator.selected].id, later_first_branch);

        app.move_branch_selection(KeyCode::Up).unwrap();
        assert_eq!(app.pending_branch_node, Some(deep_child));

        app.move_branch_selection(KeyCode::Down).unwrap();
        assert_eq!(app.pending_branch_node, Some(later_first_branch));

        app.move_branch_selection(KeyCode::Down).unwrap();
        assert_eq!(app.pending_branch_node, Some(second_branch));
    }

    #[test]
    fn branch_navigator_colors_continuing_preview_edges_coral_across_other_subtrees() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, deep_child, later_first_branch, _) =
            interleaved_branching_test_app(root.path());
        app.open_branch_navigator();
        let navigator = app.branch_navigator.as_ref().unwrap();
        let deep_row = navigator
            .rows
            .iter()
            .find(|row| row.id == deep_child)
            .unwrap();
        let line = branch_row_line(
            deep_row,
            later_first_branch,
            &navigator.preview_path,
            &navigator.original_path,
        );

        assert_eq!(line.spans[1].content.as_ref(), "┃ ");
        assert_eq!(line.spans[1].style.fg, Some(CRAB));
    }

    #[test]
    fn branch_navigator_keeps_selected_third_sibling_trunk_coral_through_earlier_subtrees() {
        let root = Uuid::from_u128(1);
        let branch_parent = Uuid::from_u128(2);
        let first_sibling = Uuid::from_u128(3);
        let first_descendant = Uuid::from_u128(4);
        let middle_sibling = Uuid::from_u128(5);
        let middle_descendant = Uuid::from_u128(6);
        let selected_sibling = Uuid::from_u128(7);
        let nodes = vec![
            ConversationGraphNode {
                id: root,
                parent_id: None,
            },
            ConversationGraphNode {
                id: branch_parent,
                parent_id: Some(root),
            },
            ConversationGraphNode {
                id: first_sibling,
                parent_id: Some(branch_parent),
            },
            ConversationGraphNode {
                id: first_descendant,
                parent_id: Some(first_sibling),
            },
            ConversationGraphNode {
                id: middle_sibling,
                parent_id: Some(branch_parent),
            },
            ConversationGraphNode {
                id: middle_descendant,
                parent_id: Some(middle_sibling),
            },
            ConversationGraphNode {
                id: selected_sibling,
                parent_id: Some(branch_parent),
            },
        ];
        let rows = branch_rows(&nodes);
        let preview_path = HashSet::from([root, branch_parent, selected_sibling]);
        let original_path = HashSet::new();

        let expected_coral_segments = [
            (first_sibling, 1, "┣"),
            (first_descendant, 1, "┃ "),
            (middle_sibling, 1, "┣"),
            (middle_descendant, 1, "┃ "),
        ];
        for (row_id, span_index, expected_symbol) in expected_coral_segments {
            let row = rows.iter().find(|row| row.id == row_id).unwrap();
            let line = branch_row_line(row, selected_sibling, &preview_path, &original_path);
            assert_eq!(line.spans[span_index].content.as_ref(), expected_symbol);
            assert_eq!(line.spans[span_index].style.fg, Some(CRAB));
        }

        let first_sibling_row = rows.iter().find(|row| row.id == first_sibling).unwrap();
        let first_sibling_line = branch_row_line(
            first_sibling_row,
            selected_sibling,
            &preview_path,
            &original_path,
        );
        assert_eq!(first_sibling_line.spans[2].content.as_ref(), "─");
        assert_eq!(first_sibling_line.spans[2].style.fg, Some(MUTED));
    }

    #[test]
    fn branch_navigator_can_load_a_selected_message_into_the_editor_and_cancel_cleanly() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, original_leaf, newer_leaf) = branching_test_app(root.path());
        app.input = "preserved draft".into();
        app.cursor = app.input.len();
        app.open_branch_navigator();
        app.move_branch_selection(KeyCode::Up).unwrap();

        app.begin_selected_message_edit();

        assert!(app.branch_navigator.is_none());
        assert_eq!(
            app.editing_message.as_ref().map(|edit| edit.node_id),
            Some(original_leaf)
        );
        assert_eq!(app.input, "original follow-up");
        assert!(app.transcript_node_ids.contains(&original_leaf));

        app.cancel_message_edit();

        assert!(app.editing_message.is_none());
        assert_eq!(app.input, "preserved draft");
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

    #[tokio::test]
    async fn arrow_up_recalls_only_the_latest_visible_user_message_and_escape_cancels() {
        let root = tempfile::tempdir().unwrap();
        let config = Config::test("auto", "http://127.0.0.1:1/v1");
        let store = SessionStore::new(root.path()).unwrap();
        let mut session = store
            .create(
                config
                    .provider(&config.active_provider)
                    .unwrap()
                    .model
                    .clone(),
            )
            .unwrap();
        session
            .messages
            .push(Message::text(Role::User, "older visible message"));
        session
            .messages
            .push(Message::text(Role::Assistant, "older answer"));
        let latest_visible = session
            .messages
            .push(Message::text(Role::User, "latest visible message"));
        session
            .messages
            .push(Message::text(Role::Assistant, "latest answer"));
        session
            .messages
            .push(Message::hidden_text(Role::User, "hidden goal continuation"));
        let mut app = test_app_with_session(root.path(), config, session);
        let original_path = app.transcript_node_ids.clone();
        app.scroll = 7;
        app.max_scroll = 20;

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await
            .unwrap();

        assert_eq!(app.input, "latest visible message");
        assert_eq!(app.cursor, app.input.len());
        assert_eq!(
            app.editing_message.as_ref().map(|edit| edit.node_id),
            Some(latest_visible)
        );
        assert_eq!(app.transcript_node_ids, original_path);
        assert_eq!(app.scroll, 7);

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.input, "latest visible message");
        assert_eq!(
            app.editing_message.as_ref().map(|edit| edit.node_id),
            Some(latest_visible)
        );
        assert_eq!(app.scroll, 7);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.input.is_empty());
        assert!(app.editing_message.is_none());
        assert_eq!(app.transcript_node_ids, original_path);
        assert_eq!(app.scroll, 7);
    }

    #[tokio::test]
    async fn composer_arrows_never_scroll_at_empty_multiline_or_soft_wrap_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.scroll = 12;
        app.max_scroll = 30;

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.scroll, 12);
        assert!(app.input.is_empty());

        app.input = "top\nbottom".into();
        app.cursor = 0;
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.cursor, 0);
        assert_eq!(app.scroll, 12);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await
            .unwrap();
        let multiline_bottom = app.cursor;
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.cursor, multiline_bottom);
        assert_eq!(app.scroll, 12);

        app.input = "abcdefgh".into();
        app.cursor = 0;
        app.composer_width = 4;
        app.preferred_column = None;
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.cursor, 0);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.cursor > 0);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await
            .unwrap();
        let wrapped_bottom = app.cursor;
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.cursor, wrapped_bottom);
        assert_eq!(app.scroll, 12);
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
        assert!(app.prompt_queue.items.is_empty());
        assert!(!app.is_running());
    }

    #[test]
    fn composer_attachment_binding_moves_and_is_removed_when_edited() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let attachment_id = Uuid::new_v4();
        app.insert("hello ");
        app.insert_clipboard_item("@preview.png", Some(attachment_id));
        assert_eq!(app.composer_attachments.len(), 1);
        assert_eq!(app.composer_attachments[0].start, 6);

        app.cursor = 0;
        app.insert("well ");
        assert_eq!(app.composer_attachments[0].start, 11);

        app.cursor = app.composer_attachments[0].start + 1;
        app.delete();
        assert!(app.composer_attachments.is_empty());
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
    async fn submitting_a_new_turn_scrolls_to_the_bottom_and_resumes_following() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.scroll = 7;
        app.max_scroll = 20;
        app.auto_scroll = false;
        app.input = "Start a new turn".into();
        app.cursor = app.input.len();

        app.submit().await.unwrap();

        assert!(app.auto_scroll);
        app.running.take().unwrap().abort();
    }

    #[tokio::test]
    async fn submitting_while_the_agent_works_queues_multiple_messages() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let (_finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
        app.running = Some(tokio::spawn(async move {
            let _ = finish_rx.await;
            anyhow::bail!("test turn remains pending")
        }));
        app.scroll = 7;
        app.max_scroll = 20;
        app.auto_scroll = false;
        app.input = "Follow up after this turn".into();
        app.cursor = app.input.len();

        app.submit().await.unwrap();
        app.input = "Then run the focused tests".into();
        app.cursor = app.input.len();
        app.submit().await.unwrap();

        assert_eq!(
            app.prompt_queue
                .items
                .iter()
                .map(|prompt| prompt.content.as_str())
                .collect::<Vec<_>>(),
            vec!["Follow up after this turn", "Then run the focused tests"]
        );
        assert!(app.input.is_empty());
        assert!(app.is_running());
        assert_eq!(app.scroll, 7);
        assert!(!app.auto_scroll);
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

        let id = app.prompt_queue.push("Steer now".into(), Vec::new());
        app.queued_prompt_buttons.push(QueuedPromptButtons {
            id,
            steer: Rect::new(10, 4, 9, 3),
            edit: Rect::new(19, 4, 7, 3),
            delete: Rect::new(26, 4, 9, 3),
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.prompt_queue.steered_id, Some(id));
        assert_eq!(app.prompt_queue.items.front().unwrap().content, "Steer now");
        app.running.take().unwrap().abort();
    }

    #[test]
    fn queued_messages_render_compact_controls_above_the_composer() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.prompt_queue
            .push("Use the other implementation".into(), Vec::new());
        app.prompt_queue
            .push("Then update its tests".into(), Vec::new());
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

        assert!(text.contains("QUEUED"));
        assert!(text.contains("Use the other implementation"));
        assert!(text.contains("Then update its tests"));
        assert!(text.contains("Steer"));
        assert!(text.contains("Edit"));
        assert!(text.contains("Delete"));
        assert_eq!(app.queued_prompt_buttons.len(), 2);

        let controls = app.queued_prompt_buttons[1];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: controls.edit.x + 1,
            row: controls.edit.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.queued_prompt_edit.as_ref().map(|edit| edit.id),
            Some(controls.id)
        );
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: controls.delete.x + 1,
            row: controls.delete.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.queued_prompt_edit.is_none());
        assert_eq!(app.prompt_queue.items.len(), 1);
    }

    #[tokio::test]
    async fn editing_and_deleting_queued_messages_preserves_queue_order() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let first = app.prompt_queue.push("first".into(), Vec::new());
        let second = app.prompt_queue.push("second".into(), Vec::new());
        let third = app.prompt_queue.push("third".into(), Vec::new());
        app.input = "unsent draft".into();
        app.cursor = app.input.len();

        app.begin_queued_prompt_edit(second);
        assert_eq!(app.input, "second");
        app.input = "edited second".into();
        app.cursor = app.input.len();
        app.submit().await.unwrap();

        assert_eq!(app.input, "unsent draft");
        assert_eq!(
            app.prompt_queue
                .items
                .iter()
                .map(|prompt| (prompt.id, prompt.content.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (first, "first"),
                (second, "edited second"),
                (third, "third")
            ]
        );

        app.delete_queued_prompt(first);
        assert_eq!(
            app.prompt_queue
                .items
                .iter()
                .map(|prompt| prompt.id)
                .collect::<Vec<_>>(),
            vec![second, third]
        );
    }

    #[tokio::test]
    async fn an_edited_front_message_waits_then_dispatches_in_place() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let first = app.prompt_queue.push("first".into(), Vec::new());
        let second = app.prompt_queue.push("second".into(), Vec::new());

        app.begin_queued_prompt_edit(first);
        assert!(!app.dispatch_queued_prompt_if_idle().unwrap());
        app.finish_queued_prompt_edit("edited first".into());
        assert!(app.dispatch_queued_prompt_if_idle().unwrap());

        assert_eq!(app.pending_user.as_deref(), Some("edited first"));
        assert_eq!(
            app.prompt_queue
                .items
                .iter()
                .map(|prompt| prompt.id)
                .collect::<Vec<_>>(),
            vec![second]
        );
        app.running.take().unwrap().abort();
    }

    #[test]
    fn prompt_queue_is_fifo_but_steer_targets_the_selected_message() {
        let mut queue = PromptQueue::default();
        let first = queue.push("first".into(), Vec::new());
        let second = queue.push("second".into(), Vec::new());
        let third = queue.push("third".into(), Vec::new());

        assert!(queue.steer(second));
        assert_eq!(queue.pop_next(None).unwrap().id, second);
        assert_eq!(queue.pop_next(None).unwrap().id, first);
        assert!(queue.pop_next(Some(third)).is_none());
        assert_eq!(queue.pop_next(None).unwrap().id, third);
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
            parts: Vec::new(),
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
                change_id: None,
                live_change_id: None,
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
                change_id: None,
                live_change_id: None,
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

    #[test]
    fn long_shell_commands_use_one_terminal_row_without_losing_the_full_detail() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let command = "cargo test --workspace --all-features --release";
        app.pending_user = Some("Run every test".into());
        app.activities.push(AgentActivity {
            id: "shell-1".into(),
            turn_message_id: Uuid::nil(),
            turn_message_index: 0,
            sequence: Some(1),
            started_at: None,
            completed_at: None,
            tool: "shell".into(),
            kind: ActivityKind::Shell,
            status: ActivityStatus::Completed,
            title: "Ran command".into(),
            detail: command.into(),
            change_id: None,
            live_change_id: None,
        });

        let source = conversation_source(&app);
        let activity_line = source
            .lines
            .iter()
            .position(|line| line.to_string().contains(command))
            .unwrap();
        assert!(source.single_row_lines.contains(&activity_line));

        let rows = wrap_conversation_lines(
            &source.lines,
            &source.user_lines,
            &source.single_row_lines,
            32,
        );
        let activity_rows = rows
            .iter()
            .filter(|row| row.source_line == activity_line)
            .collect::<Vec<_>>();
        assert_eq!(activity_rows.len(), 1);
        let collapsed = activity_rows[0]
            .units
            .iter()
            .map(|unit| unit.text.as_str())
            .collect::<String>();
        assert!(collapsed.contains("cargo test"));
        assert!(collapsed.ends_with('…'));
        assert!(
            source
                .copy_targets
                .iter()
                .any(|target| target.text == command)
        );

        let full_line = source.lines[activity_line].to_string();
        let wide_rows = wrap_conversation_lines(
            &source.lines,
            &source.user_lines,
            &source.single_row_lines,
            display_width(&full_line) as u16,
        );
        let wide_activity = wide_rows
            .iter()
            .find(|row| row.source_line == activity_line)
            .unwrap()
            .units
            .iter()
            .map(|unit| unit.text.as_str())
            .collect::<String>();
        assert_eq!(wide_activity, full_line);
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
            change_id: None,
            live_change_id: None,
        });
        app.turns.push(AgentTurn {
            message_id: Uuid::nil(),
            message_index: 0,
            started_at,
            completed_at: completed.then_some(started_at + chrono::Duration::seconds(7)),
            outcome: completed.then_some(crate::session::TurnOutcome::Completed),
            change_id: None,
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
        let rows = wrap_conversation_lines(
            &source.lines,
            &source.user_lines,
            &source.single_row_lines,
            80,
        );
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
        let rows = wrap_conversation_lines(&lines, &HashSet::new(), &HashSet::new(), 3);
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
            user_message: false,
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
                    user_message: false,
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

    fn write_test_skill(root: &Path) -> PathBuf {
        let skill = root.join(".agents/skills/review-rust");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: review-rust\ndescription: Review Rust changes.\n---\nReview the code.",
        )
        .unwrap();
        skill
    }

    #[test]
    fn slash_menu_combines_commands_and_skills_only_at_the_start() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        write_test_skill(root.path());

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

        app.clear_composer_text();
        app.close_completion();
        app.insert("Review this /");
        let menu = app.completion.as_ref().unwrap();
        assert!(
            menu.items
                .iter()
                .all(|item| item.kind == CompletionKind::Skill)
        );
    }

    #[test]
    fn opening_slash_completion_refreshes_terminal_skills_once_for_that_menu() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let skill = write_test_skill(root.path());

        assert!(app.handle_paste("Please /"));
        assert!(app.skills.iter().any(|skill| skill.name == "review-rust"));
        assert!(
            app.completion
                .as_ref()
                .unwrap()
                .items
                .iter()
                .any(|item| item.name == "review-rust")
        );

        std::fs::remove_file(skill.join("SKILL.md")).unwrap();
        app.insert_char('r');
        assert!(app.skills.iter().any(|skill| skill.name == "review-rust"));

        app.close_completion();
        app.move_left();
        assert!(app.skills.iter().all(|skill| skill.name != "review-rust"));
    }

    #[tokio::test]
    async fn accepting_a_skill_completion_inserts_a_slash_mention() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        write_test_skill(root.path());
        app.insert("Please /rev");

        assert!(app.accept_completion().await);
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
            change_id: None,
            live_change_id: None,
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
            parts: Vec::new(),
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
            parts: Vec::new(),
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
            change_id: None,
            live_change_id: None,
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
                change_id: None,
                live_change_id: None,
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
                change_id: None,
                live_change_id: None,
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
            change_id: None,
            live_change_id: None,
        };

        assert_eq!(
            activity_detail_for_display(root.path(), &activity),
            "src/main.rs"
        );
    }

    #[test]
    fn user_message_background_spans_compact_and_wide_rows_only() {
        for width in [24, 80] {
            let root = tempfile::tempdir().unwrap();
            let mut app = test_app(root.path());
            app.transcript
                .push(Message::text(Role::User, "A visible user message"));
            app.transcript
                .push(Message::text(Role::Assistant, "Assistant answer"));
            let backend = TestBackend::new(width, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| render_chat(frame, &mut app, frame.area()))
                .unwrap();

            let view = app.conversation_view.as_ref().unwrap();
            let buffer = terminal.backend().buffer();
            for (row_index, row) in view.rows.iter().enumerate() {
                let viewport_row = row_index.checked_sub(view.scroll);
                let Some(y) = viewport_row.map(|row| row as u16 + 1) else {
                    continue;
                };
                if y >= 11 {
                    continue;
                }
                for x in 0..width {
                    if row.user_message {
                        assert_eq!(
                            buffer[(x, y)].bg,
                            USER_MESSAGE_BG,
                            "incomplete user background at width {width}, row {row_index}, x {x}"
                        );
                    } else {
                        assert_ne!(
                            buffer[(x, y)].bg,
                            USER_MESSAGE_BG,
                            "user background leaked at width {width}, row {row_index}, x {x}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn wrapped_and_multiline_user_messages_keep_a_continuous_full_width_background() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.transcript.push(Message::text(
            Role::User,
            "This first logical line wraps across several compact terminal rows.\nSecond line.",
        ));
        let width = 20;
        let backend = TestBackend::new(width, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_chat(frame, &mut app, frame.area()))
            .unwrap();

        let view = app.conversation_view.as_ref().unwrap();
        let user_rows = view
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.user_message)
            .collect::<Vec<_>>();
        assert!(user_rows.len() >= 5);
        assert!(user_rows.windows(2).all(|rows| rows[1].0 == rows[0].0 + 1));
        let buffer = terminal.backend().buffer();
        for (row_index, _) in user_rows {
            let y = (row_index - view.scroll) as u16 + 1;
            assert!((0..width).all(|x| buffer[(x, y)].bg == USER_MESSAGE_BG));
        }
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

    #[tokio::test]
    async fn renders_and_scrolls_provider_defined_openai_usage_at_wide_and_compact_sizes() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.usage_open = true;
        app.usage_state = UsageState {
            available: true,
            stale: false,
            can_reset: true,
            last_updated_at: Some(1_786_826_000),
            snapshot: Some(UsageSnapshot {
                plan_type: "pro".into(),
                windows: vec![UsageWindow {
                    limit_id: "codex".into(),
                    limit_name: None,
                    kind: UsageWindowKind::Primary,
                    used_percent: 37.0,
                    remaining_percent: 63.0,
                    window_duration_seconds: 604_800,
                    resets_at: 1_786_826_526,
                }],
                reset_credits: ResetCredits {
                    available_count: 2,
                    applicable_available_count: 1,
                    credits: (1..=3)
                        .map(|index| ResetCredit {
                            id: format!("credit-{index}"),
                            reset_type: "codex_rate_limits".into(),
                            status: "available".into(),
                            granted_at: "2026-06-17T00:00:00Z".into(),
                            expires_at: Some("2026-07-17T00:00:00Z".into()),
                            title: Some(format!("Reset credit {index}")),
                            description: Some("Ready to redeem".into()),
                        })
                        .collect(),
                },
            }),
            error: None,
        };

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
        assert!(rendered.contains("OpenAI ChatGPT plan usage"));
        assert!(rendered.contains("37% used / 63% remaining"));
        assert!(rendered.contains("1 week window"));
        assert!(rendered.contains("Manual resets: 2 available"));
        assert!(rendered.contains("Enter reset usage"));

        app.usage_confirm = true;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("consumes 1 credit (1 remaining)"));
        assert!(rendered.contains("cannot be undone"));

        app.usage_state.can_reset = false;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let paused = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(paused.contains("confirmation paused"));
        app.usage_state.can_reset = true;

        let compact_backend = TestBackend::new(58, 14);
        let mut compact_terminal = Terminal::new(compact_backend).unwrap();
        compact_terminal
            .draw(|frame| render(frame, &mut app))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.usage_scroll, 1);
        compact_terminal
            .draw(|frame| render(frame, &mut app))
            .unwrap();
        let compact = compact_terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(compact.contains("Plan: pro"));
    }

    #[tokio::test]
    async fn usage_command_opens_during_an_active_turn_instead_of_queuing() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let provider = app
            .config
            .providers
            .get_mut(&app.provider)
            .expect("active provider");
        provider.base_url = crate::config::OFFICIAL_OPENAI_BASE_URL.into();
        provider.auth = "oauth".into();
        app.usage_tracker = UsageTracker::test(true, None).unwrap();
        app.usage_state = UsageState::empty();
        app.running = Some(tokio::spawn(std::future::pending()));
        app.usage_task = Some(tokio::spawn(std::future::pending()));
        app.input = "/usage".into();
        app.cursor = app.input.len();

        app.submit().await.unwrap();

        assert!(app.usage_open);
        assert!(app.input.is_empty());
        assert!(app.prompt_queue.items.is_empty());
        app.running.take().unwrap().abort();
        app.usage_task.take().unwrap().abort();
    }

    #[tokio::test]
    async fn processes_command_opens_during_an_active_turn_and_owns_input() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.running = Some(tokio::spawn(std::future::pending()));
        app.input = "/processes".into();
        app.cursor = app.input.len();

        app.submit().await.unwrap();

        assert!(app.process_dialog.is_some());
        assert!(app.input.is_empty());
        assert!(app.prompt_queue.items.is_empty());
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Processes · 0 running"));
        assert!(text.contains("No managed terminals are running"));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.process_dialog.is_none());
        app.running.take().unwrap().abort();
    }

    #[tokio::test]
    async fn a_parked_openai_turn_requests_usage_refresh_when_it_finishes() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let provider = app
            .config
            .providers
            .get_mut(&app.provider)
            .expect("active provider");
        provider.base_url = crate::config::OFFICIAL_OPENAI_BASE_URL.into();
        provider.auth = "oauth".into();
        app.usage_tracker = UsageTracker::test(true, Some("http://127.0.0.1:1".into())).unwrap();
        app.usage_state = UsageState::empty();
        let snapshot = app.conversation.snapshot();
        app.running = Some(tokio::spawn(async move {
            Ok(ConversationTurn {
                result: Ok("finished test turn".into()),
                snapshot,
            })
        }));
        app.event_rx = Some(mpsc::unbounded_channel().1);
        let id = app.conversation.snapshot().session.id;
        app.park_current_turn();
        tokio::task::yield_now().await;

        app.refresh_usage_for_finished_background_turns().await;

        assert!(
            app.background_turns
                .values()
                .all(|background| background.usage_refresh_requested)
        );
        assert!(app.usage_task.is_some());
        app.restore_turn_state(id);
        app.finish_turn_if_ready().await.unwrap();
        assert!(!app.usage_refresh_pending);
        app.usage_task.take().unwrap().abort();
    }

    #[tokio::test]
    async fn a_parked_failed_turn_does_not_refresh_usage_or_refresh_again_after_restore() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        let provider = app
            .config
            .providers
            .get_mut(&app.provider)
            .expect("active provider");
        provider.base_url = crate::config::OFFICIAL_OPENAI_BASE_URL.into();
        provider.auth = "oauth".into();
        app.usage_tracker = UsageTracker::test(true, Some("http://127.0.0.1:1".into())).unwrap();
        app.usage_state = UsageState::empty();
        let snapshot = app.conversation.snapshot();
        app.running = Some(tokio::spawn(async move {
            Ok(ConversationTurn {
                result: Err(anyhow::anyhow!("failed test turn")),
                snapshot,
            })
        }));
        app.event_rx = Some(mpsc::unbounded_channel().1);
        let id = app.conversation.snapshot().session.id;
        app.park_current_turn();
        tokio::task::yield_now().await;

        app.refresh_usage_for_finished_background_turns().await;
        assert!(app.usage_task.is_none());

        app.restore_turn_state(id);
        app.finish_turn_if_ready().await.unwrap();
        assert!(app.usage_task.is_none());
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
        write_test_skill(root.path());
        for width in [100, 36] {
            let mut app = test_app(root.path());
            app.insert("/");
            let backend = TestBackend::new(width, 30);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();

            assert!(text.contains("Slash menu"), "missing menu at {width}");
            assert!(text.contains("/help"), "missing command at {width}");

            app.input.clear();
            app.cursor = 0;
            app.completion = None;
            app.insert("/review");
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let filtered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                filtered.contains("/review-rust"),
                "missing filtered skill at {width}"
            );
        }
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
    fn recording_keys_cancel_or_send_without_conflicting_with_modified_enter() {
        assert_eq!(
            recording_key_action(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            RecordingKeyAction::Cancel
        );
        assert_eq!(
            recording_key_action(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            RecordingKeyAction::StopAndSend
        );
        assert_eq!(
            recording_key_action(&KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            RecordingKeyAction::Ignore
        );
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

    #[tokio::test]
    async fn at_menu_completes_files_folders_and_parent_paths() {
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
        assert!(!rendered.contains(" folder "));
        assert!(!rendered.contains(" file "));

        app.completion.as_mut().unwrap().selected = hello;
        assert!(app.accept_completion().await);
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

    #[test]
    fn at_menu_resets_selection_when_the_composer_changes() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("alpha.txt"), "").unwrap();
        fs::write(temp.path().join("alpine.txt"), "").unwrap();
        let mut app = test_app(temp.path());

        app.insert("@a");
        app.completion.as_mut().unwrap().selected = 1;
        app.insert("l");

        assert_eq!(app.completion.as_ref().unwrap().selected, 0);
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
        assert!(!text.contains("openai future-9-sol"));
        let after_model = text.split_once("future-9-sol").unwrap().1;
        let (before_fast, after_fast) = after_model.split_once('⚡').unwrap();
        assert!(!before_fast.contains('│'));
        let before_thinking = after_fast.split_once("deep").unwrap().0;
        assert!(!before_thinking.contains('│'));
    }

    #[tokio::test]
    async fn model_picker_skips_speed_when_only_standard_is_available() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.model_catalog = vec![ModelCatalogEntry {
            slug: "custom-reasoning-model".into(),
            display_name: "Custom reasoning model".into(),
            default_reasoning_level: Some("high".into()),
            supported_reasoning_levels: vec![ReasoningOption {
                effort: "high".into(),
                name: "high".into(),
                description: "Deeper reasoning".into(),
            }],
            service_tiers: vec![ServiceTierOption {
                id: "default".into(),
                name: "Standard".into(),
                description: "Normal speed".into(),
            }],
            default_service_tier: Some("default".into()),
            ..ModelCatalogEntry::from_id("custom-reasoning-model".into())
        }];

        app.open_model_picker();
        app.accept_model_selection().await.unwrap();
        assert_eq!(
            app.model_picker.as_ref().unwrap().step,
            ModelPickerStep::Reasoning
        );
        app.accept_model_selection().await.unwrap();

        assert!(app.model_picker.is_none());
        assert_eq!(app.model, "custom-reasoning-model");
        assert_eq!(app.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(app.service_tier, None);
        assert_eq!(
            app.conversation
                .snapshot()
                .session
                .reasoning_effort
                .as_deref(),
            Some("high")
        );
    }

    #[tokio::test]
    async fn provider_picker_switches_only_the_current_session_and_rolls_back_without_models() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::test("old-model", "http://127.0.0.1:1/v1");
        let mut local = ProviderConfig::test("local-model".into(), "http://127.0.0.1:2/v1".into());
        local.fetch_models = false;
        local
            .model_capabilities
            .insert("local-model".into(), ModelCapabilitiesConfig::default());
        config.providers.insert("local".into(), local);
        let mut empty = ProviderConfig::test("auto".into(), "http://127.0.0.1:3/v1".into());
        empty.fetch_models = false;
        config.providers.insert("empty".into(), empty);
        let session = SessionStore::new(root.path())
            .unwrap()
            .create_for_provider(config.active_provider.clone(), "old-model".into())
            .unwrap();
        let session_id = session.id;
        let mut app = test_app_with_session(root.path(), config, session);

        app.open_provider_picker();
        let initial_selection = app.provider_picker.as_ref().unwrap().selected;
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_ne!(
            app.provider_picker.as_ref().unwrap().selected,
            initial_selection
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.provider_picker.is_none());

        app.open_provider_picker();
        let local_index = app
            .provider_picker
            .as_ref()
            .unwrap()
            .providers
            .iter()
            .position(|provider| provider == "local")
            .unwrap();
        app.provider_picker.as_mut().unwrap().selected = local_index;
        app.accept_provider_selection().await.unwrap();

        assert!(app.provider_picker.is_none());
        assert_eq!(app.provider, "local");
        assert_eq!(app.model, "local-model");
        assert_eq!(app.config.active_provider, crate::config::DEFAULT_PROVIDER);
        let persisted = SessionStore::new(root.path())
            .unwrap()
            .load(Some(&session_id.to_string()))
            .unwrap();
        assert_eq!(persisted.provider, "local");
        assert_eq!(persisted.model, "local-model");

        app.open_provider_picker();
        let empty_index = app
            .provider_picker
            .as_ref()
            .unwrap()
            .providers
            .iter()
            .position(|provider| provider == "empty")
            .unwrap();
        app.provider_picker.as_mut().unwrap().selected = empty_index;
        app.accept_provider_selection().await.unwrap();

        assert_eq!(app.provider, "local");
        assert_eq!(app.model, "local-model");
        assert_eq!(
            app.provider_picker
                .as_ref()
                .and_then(|picker| picker.notice.as_deref()),
            Some(crate::agent::NO_PROVIDER_MODELS_MESSAGE)
        );
    }

    #[test]
    fn provider_picker_renders_at_wide_and_compact_sizes() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.config.providers.insert(
            "local".into(),
            ProviderConfig::test("local-model".into(), "http://localhost:11434/v1".into()),
        );
        app.open_provider_picker();
        app.provider_picker.as_mut().unwrap().notice =
            Some(crate::agent::NO_PROVIDER_MODELS_MESSAGE.into());

        for (width, height) in [(100, 24), (50, 12)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains("Providers"));
            assert!(rendered.contains("local"));
            assert!(rendered.contains("No models were found"));
        }
    }

    #[test]
    fn header_prefixes_the_unified_model_section_when_multiple_providers_exist() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(root.path());
        app.config
            .providers
            .insert("local".into(), crate::config::ProviderConfig::default());
        app.model = "future-model".into();
        app.reasoning_effort = Some("high".into());
        app.service_tier = Some("standard".into());
        app.model_catalog = vec![ModelCatalogEntry {
            service_tiers: vec![ServiceTierOption {
                id: "standard".into(),
                name: "Standard".into(),
                description: "Normal priority".into(),
            }],
            ..ModelCatalogEntry::from_id(app.model.clone())
        }];

        let backend = TestBackend::new(100, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_header(frame, &app, area);
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("openai future-model high"));
        let model_section = text.split_once("openai").unwrap().1;
        let model_section = model_section.split_once('│').unwrap().0;
        assert!(!model_section.contains('⚡'));
    }

    #[tokio::test]
    async fn no_project_command_switches_scope_and_uses_global_session_storage() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        fs::create_dir(&root).unwrap();
        let mut app = test_app(&root);

        app.create_no_project_session().await.unwrap();

        let snapshot = app.conversation.snapshot();
        assert_eq!(snapshot.session.scope, SessionScope::NoProject);
        assert_eq!(app.project, "No project");
        assert!(paths_equal(&snapshot.project_root, &root));
        let store = SessionStore::no_project_at(&app.registry.data_dir().unwrap()).unwrap();
        assert_eq!(
            store
                .load(Some(&snapshot.session.id.to_string()))
                .unwrap()
                .scope,
            SessionScope::NoProject
        );

        app.open_session_picker().await.unwrap();
        let row = app
            .session_picker
            .as_ref()
            .unwrap()
            .rows()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    SessionPickerRow::Session(project, section, session)
                        if app.session_picker.as_ref().unwrap().projects[*project].sessions(*section)[*session].id
                            == snapshot.session.id
                )
            })
            .unwrap();
        app.session_picker.as_mut().unwrap().selected = row;
        app.delete_session_selection().await.unwrap();
        assert!(!app.active_session);
        assert!(app.transcript.is_empty());
        assert!(store.list().unwrap().is_empty());
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
        let initial_session_id = app.conversation.snapshot().session.id;
        app.registry = registry.clone();
        let mut saved = other_store.create("restored-model".into()).unwrap();
        saved.reasoning_effort = Some("high".into());
        saved.service_tier = Some("priority".into());
        saved.title = "Saved conversation".into();
        saved
            .messages
            .push(Message::text(Role::User, "Remember this"));
        other_store.save(&saved).unwrap();
        let mut child = other_store.create("restored-model".into()).unwrap();
        child.parent_session_id = Some(saved.id);
        child.title = "Child session".into();
        other_store.save(&child).unwrap();
        let mut grandchild = other_store.create("restored-model".into()).unwrap();
        grandchild.parent_session_id = Some(child.id);
        grandchild.title = "Grandchild session".into();
        other_store.save(&grandchild).unwrap();
        registry.register(&other_root).unwrap();

        app.open_session_picker().await.unwrap();
        let picker = app.session_picker.as_ref().unwrap();
        assert!(picker.projects.iter().any(|project| {
            project
                .project
                .root
                .as_deref()
                .is_some_and(|root| paths_equal(root, &current_root))
        }));
        let current_project = picker
            .projects
            .iter()
            .position(|project| {
                project
                    .project
                    .root
                    .as_deref()
                    .is_some_and(|root| paths_equal(root, &current_root))
            })
            .unwrap();
        let other_project = picker
            .projects
            .iter()
            .position(|project| {
                project
                    .project
                    .root
                    .as_deref()
                    .is_some_and(|root| paths_equal(root, &other_root))
            })
            .unwrap();
        assert!(picker.projects[current_project].expanded);
        assert!(matches!(
            picker.selected_row(),
            Some(SessionPickerRow::Session(project, _, _)) if project == current_project
        ));

        app.move_session_left();
        assert!(matches!(
            app.session_picker.as_ref().unwrap().selected_row(),
            Some(SessionPickerRow::Project(project)) if project == current_project
        ));
        app.move_session_left();
        assert!(!app.session_picker.as_ref().unwrap().projects[current_project].expanded);
        app.move_session_selection(1);
        assert!(matches!(
            app.session_picker.as_ref().unwrap().selected_row(),
            Some(SessionPickerRow::Project(project)) if project == other_project
        ));
        app.move_session_right();
        assert!(app.session_picker.as_ref().unwrap().projects[other_project].expanded);
        app.move_session_selection(1);
        assert!(matches!(
            app.session_picker.as_ref().unwrap().selected_row(),
            Some(SessionPickerRow::Session(project, _, _)) if project == other_project
        ));
        let picker = app.session_picker.as_ref().unwrap();
        let selected = picker.selected_row().unwrap();
        let SessionPickerRow::Session(_, section, selected_index) = selected else {
            unreachable!();
        };
        assert_eq!(
            picker.projects[other_project].sessions(section)[selected_index].id,
            saved.id
        );
        assert_eq!(
            picker.projects[other_project].sessions(section)[selected_index].descendant_count,
            2
        );

        app.move_session_left();
        assert!(
            app.session_picker
                .as_ref()
                .unwrap()
                .collapsed_sessions
                .contains(&saved.id)
        );
        assert_eq!(
            app.session_picker
                .as_ref()
                .unwrap()
                .rows()
                .iter()
                .filter(|row| matches!(row, SessionPickerRow::Session(project, _, _) if *project == other_project))
                .count(),
            1
        );

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
        assert!(text.contains("+2"));
        assert!(text.contains(" C "));
        assert!(text.contains(" U "));
        assert!(text.contains("Del delete"));

        let compact_backend = TestBackend::new(58, 18);
        let mut compact_terminal = Terminal::new(compact_backend).unwrap();
        compact_terminal
            .draw(|frame| render(frame, &mut app))
            .unwrap();
        let compact_text = compact_terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(compact_text.contains("Saved conversation"));
        assert!(compact_text.contains("+2"));

        app.move_session_right();
        app.move_session_selection(1);
        assert!(matches!(
            app.session_picker.as_ref().unwrap().selected_row(),
            Some(SessionPickerRow::Session(project, section, session))
                if project == other_project && app.session_picker.as_ref().unwrap().projects[other_project].sessions(section)[session].id == child.id
        ));
        app.move_session_left();
        assert!(
            app.session_picker
                .as_ref()
                .unwrap()
                .collapsed_sessions
                .contains(&child.id)
        );
        app.move_session_left();
        assert!(matches!(
            app.session_picker.as_ref().unwrap().selected_row(),
            Some(SessionPickerRow::Session(project, section, session))
                if project == other_project && app.session_picker.as_ref().unwrap().projects[other_project].sessions(section)[session].id == saved.id
        ));

        app.accept_session_selection().await.unwrap();
        assert!(
            SessionStore::new(&current_root)
                .unwrap()
                .load(Some(&initial_session_id.to_string()))
                .is_err()
        );
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
                        SessionPickerRow::Session(project, section, session)
                            if picker.projects[*project].sessions(*section)[*session].id == saved.id
                    )
                })
                .unwrap()
        };
        app.session_picker.as_mut().unwrap().selected = saved_row;
        app.delete_session_selection().await.unwrap();

        let replacement = app.conversation.snapshot().session;
        assert!([child.id, grandchild.id].contains(&replacement.id));
        assert_eq!(replacement.model, "restored-model");
        assert_eq!(replacement.reasoning_effort, None);
        assert_eq!(replacement.service_tier, None);
        assert!(replacement.messages.is_empty());
        assert!(other_store.load(Some(&saved.id.to_string())).is_err());
        assert!(app.session_picker.is_some());
    }

    #[tokio::test]
    async fn session_picker_shortcuts_edit_persistent_metadata_without_changing_recency() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(&root).unwrap();
        let mut app = test_app(&root);
        let id = app.conversation.snapshot().session.id;
        let updated_at = app.conversation.snapshot().session.updated_at;
        app.open_session_picker().await.unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
            .await
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .await
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .await
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        for character in "Renamed session".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .await
                .unwrap();
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();

        let persisted = SessionStore::new(&root)
            .unwrap()
            .load(Some(&id.to_string()))
            .unwrap();
        assert_eq!(persisted.title, "Renamed session");
        assert!(persisted.manual_title);
        assert!(persisted.pinned_at.is_some());
        assert!(persisted.archived_at.is_some());
        assert_eq!(persisted.updated_at, updated_at);
        let picker = app.session_picker.as_ref().unwrap();
        assert!(picker.projects.iter().any(|project| {
            project.archived_expanded
                && project
                    .archived_sessions
                    .iter()
                    .any(|session| session.id == id)
        }));
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
        assert!(text.contains("Archived"));
        assert!(text.contains("Renamed session"));
        assert!(text.contains("󰐃"));
        assert!(text.contains("󰀼"));
    }
}
