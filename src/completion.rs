use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use ignore::{
    Match, WalkBuilder,
    gitignore::{Gitignore, GitignoreBuilder},
};
use serde::Serialize;
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinHandle,
};

pub(crate) const COMMANDS: &[(&str, &str)] = &[
    ("help", "Open keyboard and command help"),
    ("models", "Choose model, reasoning, and speed"),
    ("skills", "Open the interactive skill picker"),
    ("sessions", "Browse, resume, or delete saved sessions"),
    ("processes", "Manage running shell terminals"),
    ("no-project", "Create a session without a project"),
    ("branches", "Browse conversation branches"),
    ("providers", "Choose the provider for the current session"),
    ("goal", "Start a persistent goal"),
    ("goals", "Browse and manage persistent goals"),
    ("usage", "Show OpenAI plan usage and reset credits"),
    ("cron", "Manage scheduled agent tasks"),
    ("quit", "Save the session and exit"),
];

pub(crate) const NERD_FOLDER: &str = "";
const NERD_FILE: &str = "";
const RECURSIVE_EXCLUDED_DIRECTORIES: &[&str] = &[".git", "target", "node_modules", "dist"];

#[derive(Clone, Copy)]
pub(crate) struct RecursiveSearchPolicy {
    pub(crate) minimum_query_characters: usize,
    pub(crate) maximum_local_entries: usize,
    pub(crate) maximum_local_results: usize,
    pub(crate) maximum_results: usize,
    pub(crate) maximum_visited_entries: usize,
    pub(crate) maximum_visited_directories: usize,
    pub(crate) maximum_depth: usize,
    pub(crate) maximum_elapsed: Duration,
    pub(crate) entries_per_batch: usize,
    pub(crate) update_interval: Duration,
    pub(crate) debounce: Duration,
    pub(crate) maximum_concurrent_searches: usize,
}

pub(crate) const RECURSIVE_SEARCH_POLICY: RecursiveSearchPolicy = RecursiveSearchPolicy {
    minimum_query_characters: 2,
    maximum_local_entries: 4_096,
    maximum_local_results: 48,
    maximum_results: 80,
    maximum_visited_entries: 12_000,
    maximum_visited_directories: 768,
    maximum_depth: 10,
    maximum_elapsed: Duration::from_millis(750),
    entries_per_batch: 192,
    update_interval: Duration::from_millis(55),
    debounce: Duration::from_millis(110),
    maximum_concurrent_searches: 2,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionKind {
    Command,
    Skill,
    File,
    Directory,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CompletionItem {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) display: String,
    pub(crate) description: String,
    pub(crate) icon: Option<&'static str>,
    pub(crate) kind: CompletionKind,
    pub(crate) replacement: String,
}

pub(crate) struct CompletionMenu {
    pub(crate) items: Vec<CompletionItem>,
    pub(crate) selected: usize,
    pub(crate) token_start: usize,
    pub(crate) token_end: usize,
}

pub(crate) struct CompletionUpdate {
    pub(crate) request_id: u64,
    pub(crate) items: Vec<CompletionItem>,
}

pub(crate) struct CompletionSearch {
    pub(crate) request_id: u64,
    pub(crate) token_start: usize,
    pub(crate) token_end: usize,
    updates: mpsc::UnboundedReceiver<CompletionUpdate>,
    cancelled: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl CompletionSearch {
    pub(crate) fn try_recv(&mut self) -> Result<CompletionUpdate, mpsc::error::TryRecvError> {
        self.updates.try_recv()
    }

    pub(crate) async fn recv(&mut self) -> Option<CompletionUpdate> {
        self.updates.recv().await
    }
}

impl Drop for CompletionSearch {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.task.abort();
    }
}

pub(crate) fn complete<'a>(
    input: &str,
    cursor: usize,
    working_directory: &Path,
    skills: impl IntoIterator<Item = (&'a str, &'a str)>,
    usage_available: bool,
) -> Option<CompletionMenu> {
    complete_with_policy(
        input,
        cursor,
        working_directory,
        skills,
        usage_available,
        false,
    )
}

pub(crate) fn complete_with_policy<'a>(
    input: &str,
    cursor: usize,
    working_directory: &Path,
    skills: impl IntoIterator<Item = (&'a str, &'a str)>,
    usage_available: bool,
    absolute_only: bool,
) -> Option<CompletionMenu> {
    if let Some(context) =
        file_completion_context_with_policy(input, cursor, working_directory, absolute_only)
    {
        let items = file_completion_items(&context);
        return (!items.is_empty()).then_some(CompletionMenu {
            items,
            selected: 0,
            token_start: context.start,
            token_end: context.end,
        });
    }

    let context = slash_completion_context(input, cursor)?;
    let mut items = Vec::new();
    if context.commands_allowed {
        items.extend(
            COMMANDS
                .iter()
                .filter(|(name, _)| *name != "usage" || usage_available)
                .filter(|(name, _)| name.starts_with(context.prefix))
                .map(|(name, description)| CompletionItem {
                    id: format!("command:{name}"),
                    name: (*name).to_owned(),
                    display: (*name).to_owned(),
                    description: (*description).to_owned(),
                    icon: None,
                    kind: CompletionKind::Command,
                    replacement: format!("/{name}"),
                }),
        );
    }
    items.extend(
        skills
            .into_iter()
            .filter(|(name, _)| name.starts_with(context.prefix))
            .map(|(name, description)| CompletionItem {
                id: format!("skill:{name}"),
                name: name.to_owned(),
                display: name.to_owned(),
                description: description.to_owned(),
                icon: None,
                kind: CompletionKind::Skill,
                replacement: format!("/{name} "),
            }),
    );
    (!items.is_empty()).then_some(CompletionMenu {
        items,
        selected: 0,
        token_start: context.start,
        token_end: context.end,
    })
}

pub(crate) fn builtin_command_from_input(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    let name = trimmed.strip_prefix('/')?;
    (input == trimmed && COMMANDS.iter().any(|(command, _)| *command == name)).then_some(trimmed)
}

pub(crate) fn goal_objective_from_input(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    if input != trimmed {
        return None;
    }
    let objective = trimmed.strip_prefix("/goal")?;
    objective
        .chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then(|| objective.trim())
        .filter(|objective| !objective.is_empty())
}

struct SlashCompletionContext<'a> {
    start: usize,
    end: usize,
    prefix: &'a str,
    commands_allowed: bool,
}

#[derive(Clone)]
pub(crate) struct FileCompletionContext {
    pub(crate) start: usize,
    pub(crate) end: usize,
    dir_prefix: String,
    name_prefix: String,
    pub(crate) directory: PathBuf,
    priority: Arc<FilePriorityIndex>,
}

struct FilePriorityIndex {
    project_root: PathBuf,
    global: Gitignore,
    rules: Vec<ScopedGitignore>,
}

struct ScopedGitignore {
    root: PathBuf,
    matcher: Gitignore,
}

impl FilePriorityIndex {
    fn minimal(root: &Path) -> Self {
        let project_root = normalize_absolute(root);
        let (global, _) = GitignoreBuilder::new(&project_root).build_global();
        Self {
            project_root,
            global,
            rules: Vec::new(),
        }
    }

    fn load(project_root: &Path) -> Self {
        let project_root = normalize_absolute(project_root);
        let (global, _) = GitignoreBuilder::new(&project_root).build_global();
        let mut gitignore_files = WalkBuilder::new(&project_root)
            .hidden(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .follow_links(false)
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .filter(|entry| entry.file_name() == ".gitignore")
            .map(|entry| entry.into_path())
            .collect::<Vec<_>>();
        gitignore_files.sort_by(|left, right| {
            path_depth(left)
                .cmp(&path_depth(right))
                .then_with(|| left.cmp(right))
        });
        let git_exclude = project_root.join(".git/info/exclude");
        let ignore_files = git_exclude
            .is_file()
            .then_some(git_exclude)
            .into_iter()
            .chain(gitignore_files);
        let rules = ignore_files
            .filter_map(|path| {
                let root = if path.ends_with(Path::new(".git/info/exclude")) {
                    project_root.clone()
                } else {
                    path.parent()?.to_path_buf()
                };
                let mut builder = GitignoreBuilder::new(&root);
                let _ = builder.add(&path);
                builder
                    .build()
                    .ok()
                    .map(|matcher| ScopedGitignore { root, matcher })
            })
            .collect();
        Self {
            project_root,
            global,
            rules,
        }
    }

    fn is_low_priority(&self, path: &Path, is_dir: bool, relative: &Path) -> bool {
        path_has_hidden_component(relative) || self.is_git_ignored(path, is_dir)
    }

    fn is_git_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let path = normalize_absolute(path);
        let Ok(relative) = path.strip_prefix(&self.project_root) else {
            return self.global.matched(path, is_dir).is_ignore();
        };
        let components = relative.components().collect::<Vec<_>>();
        let mut current = self.project_root.clone();
        for (index, component) in components.iter().enumerate() {
            current.push(component.as_os_str());
            let current_is_dir = index + 1 < components.len() || is_dir;
            let mut ignored = match self.global.matched(&current, current_is_dir) {
                Match::Ignore(_) => true,
                Match::Whitelist(_) | Match::None => false,
            };
            for rule in &self.rules {
                if !current.starts_with(&rule.root) {
                    continue;
                }
                match rule.matcher.matched(&current, current_is_dir) {
                    Match::Ignore(_) => ignored = true,
                    Match::Whitelist(_) => ignored = false,
                    Match::None => {}
                }
            }
            if ignored {
                return true;
            }
        }
        false
    }
}

fn file_priority_index(project_root: &Path) -> Arc<FilePriorityIndex> {
    static INDEXES: OnceLock<Mutex<HashMap<PathBuf, Arc<FilePriorityIndex>>>> = OnceLock::new();
    let project_root = normalize_absolute(project_root);
    let indexes = INDEXES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut indexes = indexes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    indexes
        .entry(project_root.clone())
        .or_insert_with(|| Arc::new(FilePriorityIndex::load(&project_root)))
        .clone()
}

fn normalize_absolute(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn path_depth(path: &Path) -> usize {
    path.components().count()
}

fn path_has_hidden_component(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

#[cfg(test)]
pub(crate) fn file_completion_context(
    input: &str,
    cursor: usize,
    working_directory: &Path,
) -> Option<FileCompletionContext> {
    file_completion_context_with_policy(input, cursor, working_directory, false)
}

pub(crate) fn file_completion_context_with_policy(
    input: &str,
    cursor: usize,
    working_directory: &Path,
    absolute_only: bool,
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
    let typed = if absolute_only && typed.is_empty() {
        filesystem_root(working_directory).display().to_string()
    } else {
        typed.to_owned()
    };
    if absolute_only && !Path::new(&typed).is_absolute() {
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
    let directory = working_directory.join(dir_prefix.replace('/', std::path::MAIN_SEPARATOR_STR));
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
        priority: if absolute_only {
            Arc::new(FilePriorityIndex::minimal(working_directory))
        } else {
            file_priority_index(working_directory)
        },
    })
}

pub(crate) fn filesystem_root(path: &Path) -> PathBuf {
    let mut root = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => root.push(component.as_os_str()),
            Component::CurDir | Component::ParentDir | Component::Normal(_) => break,
        }
    }
    if root.as_os_str().is_empty() {
        PathBuf::from(std::path::MAIN_SEPARATOR_STR)
    } else {
        root
    }
}

pub(crate) fn complete_progressive<'a>(
    input: &str,
    cursor: usize,
    working_directory: &Path,
    skills: impl IntoIterator<Item = (&'a str, &'a str)>,
    usage_available: bool,
    request_id: u64,
    absolute_only: bool,
) -> (Option<CompletionMenu>, Option<CompletionSearch>) {
    if let Some(context) =
        file_completion_context_with_policy(input, cursor, working_directory, absolute_only)
    {
        let items = file_completion_items(&context);
        let menu = (!items.is_empty()).then(|| CompletionMenu {
            items: items.clone(),
            selected: 0,
            token_start: context.start,
            token_end: context.end,
        });
        let search = start_recursive_file_completion(context, items, request_id);
        return (menu, search);
    }
    (
        complete(input, cursor, working_directory, skills, usage_available),
        None,
    )
}

pub(crate) fn recursive_file_completion_available(
    input: &str,
    cursor: usize,
    working_directory: &Path,
    absolute_only: bool,
) -> bool {
    file_completion_context_with_policy(input, cursor, working_directory, absolute_only)
        .is_some_and(|context| {
            context.name_prefix.chars().count() >= RECURSIVE_SEARCH_POLICY.minimum_query_characters
        })
}

pub(crate) fn start_file_completion_search(
    input: &str,
    cursor: usize,
    working_directory: &Path,
    request_id: u64,
    absolute_only: bool,
) -> Option<CompletionSearch> {
    let context =
        file_completion_context_with_policy(input, cursor, working_directory, absolute_only)?;
    let local_items = file_completion_items(&context);
    start_recursive_file_completion(context, local_items, request_id)
}

fn file_completion_items(context: &FileCompletionContext) -> Vec<CompletionItem> {
    let Ok(entries) = fs::read_dir(&context.directory) else {
        return Vec::new();
    };
    let query = context.name_prefix.to_lowercase();
    let mut items = entries
        .take(RECURSIVE_SEARCH_POLICY.maximum_local_entries)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let normalized_name = name.to_lowercase();
            let rank = if normalized_name == query {
                0
            } else if normalized_name.starts_with(&query) {
                1
            } else if normalized_name.contains(&query) {
                2
            } else {
                return None;
            };
            let file_type = entry.file_type().ok()?;
            let is_dir = file_type.is_dir() || (file_type.is_symlink() && entry.path().is_dir());
            let kind = if is_dir {
                CompletionKind::Directory
            } else {
                CompletionKind::File
            };
            let completion_name = format!("{}{}", context.dir_prefix, name);
            let low_priority = context.priority.is_low_priority(
                &normalize_absolute(&entry.path()),
                is_dir,
                Path::new(&completion_name),
            );
            let suffix = if is_dir { "/" } else { " " };
            let replacement = format!("@{completion_name}{suffix}");
            Some((
                low_priority,
                rank,
                CompletionItem {
                    id: format!("path:{replacement}"),
                    replacement,
                    name: completion_name,
                    display: name,
                    description: String::new(),
                    icon: Some(if is_dir {
                        NERD_FOLDER
                    } else {
                        file_icon(&entry.path())
                    }),
                    kind,
                },
            ))
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        let left_dir = left.2.kind == CompletionKind::Directory;
        let right_dir = right.2.kind == CompletionKind::Directory;
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| right_dir.cmp(&left_dir))
            .then_with(|| left.2.name.to_lowercase().cmp(&right.2.name.to_lowercase()))
            .then_with(|| left.2.name.cmp(&right.2.name))
    });
    items
        .into_iter()
        .map(|(_, _, item)| item)
        .take(RECURSIVE_SEARCH_POLICY.maximum_local_results)
        .collect()
}

#[derive(Clone)]
struct RecursiveCandidate {
    item: CompletionItem,
    low_priority: bool,
    score: i64,
    depth: usize,
}

fn start_recursive_file_completion(
    context: FileCompletionContext,
    local_items: Vec<CompletionItem>,
    request_id: u64,
) -> Option<CompletionSearch> {
    start_recursive_file_completion_with_policy(
        context,
        local_items,
        request_id,
        RECURSIVE_SEARCH_POLICY,
    )
}

fn start_recursive_file_completion_with_policy(
    context: FileCompletionContext,
    local_items: Vec<CompletionItem>,
    request_id: u64,
    policy: RecursiveSearchPolicy,
) -> Option<CompletionSearch> {
    if context.name_prefix.chars().count() < policy.minimum_query_characters {
        return None;
    }
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let task_cancelled = cancelled.clone();
    let semaphore = recursive_search_semaphore();
    let token_start = context.start;
    let token_end = context.end;
    let task = runtime.spawn(async move {
        tokio::time::sleep(policy.debounce).await;
        if task_cancelled.load(Ordering::Acquire) || updates_tx.is_closed() {
            return;
        }
        let Ok(_permit) = semaphore.acquire_owned().await else {
            return;
        };
        if task_cancelled.load(Ordering::Acquire) || updates_tx.is_closed() {
            return;
        }
        let _ = tokio::task::spawn_blocking(move || {
            scan_recursive_files(
                &context,
                &local_items,
                request_id,
                policy,
                &task_cancelled,
                &updates_tx,
            );
        })
        .await;
    });
    Some(CompletionSearch {
        request_id,
        token_start,
        token_end,
        updates: updates_rx,
        cancelled,
        task,
    })
}

fn recursive_search_semaphore() -> Arc<Semaphore> {
    static SEARCHES: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEARCHES
        .get_or_init(|| {
            Arc::new(Semaphore::new(
                RECURSIVE_SEARCH_POLICY.maximum_concurrent_searches,
            ))
        })
        .clone()
}

fn scan_recursive_files(
    context: &FileCompletionContext,
    local_items: &[CompletionItem],
    request_id: u64,
    policy: RecursiveSearchPolicy,
    cancelled: &AtomicBool,
    updates: &mpsc::UnboundedSender<CompletionUpdate>,
) {
    let started = Instant::now();
    let mut stack = vec![(context.directory.clone(), String::new(), 0_usize)];
    let mut candidates = Vec::<RecursiveCandidate>::new();
    let mut visited_entries = 0_usize;
    let mut visited_directories = 0_usize;
    let mut entries_since_update = 0_usize;
    let mut last_update = Instant::now();
    let mut candidates_changed = false;

    while let Some((directory, relative_prefix, depth)) = stack.pop() {
        if search_budget_exhausted(
            cancelled,
            updates,
            started,
            visited_entries,
            visited_directories,
            policy,
        ) || depth > policy.maximum_depth
        {
            break;
        }
        visited_directories += 1;
        let remaining = policy
            .maximum_visited_entries
            .saturating_sub(visited_entries);
        let Ok(read_dir) = fs::read_dir(&directory) else {
            continue;
        };
        let mut entries = read_dir
            .take(remaining)
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            let left = left.file_name().to_string_lossy().to_lowercase();
            let right = right.file_name().to_string_lossy().to_lowercase();
            left.cmp(&right)
        });
        let mut child_directories = Vec::new();

        for entry in entries {
            if search_budget_exhausted(
                cancelled,
                updates,
                started,
                visited_entries,
                visited_directories,
                policy,
            ) {
                break;
            }
            visited_entries += 1;
            entries_since_update += 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if relative_prefix.is_empty() {
                name.clone()
            } else {
                format!("{relative_prefix}/{name}")
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let is_symlink = file_type.is_symlink();
            let is_directory = file_type.is_dir() || (is_symlink && entry.path().is_dir());
            let entry_depth = depth + 1;

            if is_directory
                && !is_symlink
                && entry_depth <= policy.maximum_depth
                && !is_recursive_excluded_directory(&name)
            {
                child_directories.push((entry.path(), relative.clone(), entry_depth));
            }

            if entry_depth > 1
                && let Some(score) =
                    recursive_match_score(&name, &relative, &context.name_prefix, entry_depth)
            {
                let completion_name = format!("{}{}", context.dir_prefix, relative);
                let suffix = if is_directory { "/" } else { " " };
                let replacement = format!("@{completion_name}{suffix}");
                candidates.push(RecursiveCandidate {
                    low_priority: context.priority.is_low_priority(
                        &normalize_absolute(&entry.path()),
                        is_directory,
                        Path::new(&relative),
                    ),
                    item: CompletionItem {
                        id: format!("path:{replacement}"),
                        name: completion_name,
                        display: relative,
                        description: String::new(),
                        icon: Some(if is_directory {
                            NERD_FOLDER
                        } else {
                            file_icon(&entry.path())
                        }),
                        kind: if is_directory {
                            CompletionKind::Directory
                        } else {
                            CompletionKind::File
                        },
                        replacement,
                    },
                    score,
                    depth: entry_depth,
                });
                candidates_changed = true;
            }

            if candidates.len() > policy.maximum_results.saturating_mul(2) {
                sort_recursive_candidates(&mut candidates);
                candidates.truncate(policy.maximum_results);
            }
            if !candidates.is_empty()
                && (entries_since_update >= policy.entries_per_batch
                    || last_update.elapsed() >= policy.update_interval)
            {
                sort_recursive_candidates(&mut candidates);
                candidates.truncate(policy.maximum_results);
                if send_completion_update(
                    local_items,
                    &candidates,
                    request_id,
                    updates,
                    policy.maximum_results,
                )
                .is_err()
                {
                    return;
                }
                candidates_changed = false;
                entries_since_update = 0;
                last_update = Instant::now();
                std::thread::yield_now();
            }
        }
        child_directories.reverse();
        stack.extend(child_directories);
    }

    sort_recursive_candidates(&mut candidates);
    candidates.truncate(policy.maximum_results);
    if !candidates.is_empty() && candidates_changed {
        let _ = send_completion_update(
            local_items,
            &candidates,
            request_id,
            updates,
            policy.maximum_results,
        );
    }
}

fn search_budget_exhausted(
    cancelled: &AtomicBool,
    updates: &mpsc::UnboundedSender<CompletionUpdate>,
    started: Instant,
    visited_entries: usize,
    visited_directories: usize,
    policy: RecursiveSearchPolicy,
) -> bool {
    cancelled.load(Ordering::Acquire)
        || updates.is_closed()
        || visited_entries >= policy.maximum_visited_entries
        || visited_directories >= policy.maximum_visited_directories
        || started.elapsed() >= policy.maximum_elapsed
}

fn send_completion_update(
    local_items: &[CompletionItem],
    candidates: &[RecursiveCandidate],
    request_id: u64,
    updates: &mpsc::UnboundedSender<CompletionUpdate>,
    maximum_results: usize,
) -> Result<(), mpsc::error::SendError<CompletionUpdate>> {
    let mut seen = HashSet::new();
    let items = local_items
        .iter()
        .chain(candidates.iter().map(|candidate| &candidate.item))
        .filter(|item| seen.insert(item.id.clone()))
        .take(maximum_results)
        .cloned()
        .collect();
    updates.send(CompletionUpdate { request_id, items })
}

fn sort_recursive_candidates(candidates: &mut [RecursiveCandidate]) {
    candidates.sort_by(|left, right| {
        let left_dir = left.item.kind == CompletionKind::Directory;
        let right_dir = right.item.kind == CompletionKind::Directory;
        left.low_priority
            .cmp(&right.low_priority)
            .then_with(|| right.score.cmp(&left.score))
            .then_with(|| left.depth.cmp(&right.depth))
            .then_with(|| right_dir.cmp(&left_dir))
            .then_with(|| {
                left.item
                    .display
                    .to_lowercase()
                    .cmp(&right.item.display.to_lowercase())
            })
            .then_with(|| left.item.display.cmp(&right.item.display))
    });
}

fn recursive_match_score(
    basename: &str,
    relative_path: &str,
    query: &str,
    depth: usize,
) -> Option<i64> {
    let basename_score = fuzzy_score(basename, query);
    let path_score = fuzzy_score(relative_path, query).map(|score| score - 450);
    basename_score
        .into_iter()
        .chain(path_score)
        .max()
        .map(|score| score - (depth.saturating_sub(1) as i64 * 70))
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<i64> {
    let candidate = candidate.to_lowercase();
    let query = query.to_lowercase();
    if query.is_empty() {
        return None;
    }
    if candidate == query {
        return Some(12_000);
    }
    if candidate.starts_with(&query) {
        return Some(11_000 - candidate.chars().count() as i64);
    }
    if let Some(position) = candidate.find(&query) {
        return Some(10_000 - position as i64 * 20 - candidate.chars().count() as i64);
    }

    let candidate_chars = candidate.chars().collect::<Vec<_>>();
    let mut search_from = 0_usize;
    let mut first = None;
    let mut previous = None;
    let mut gaps = 0_usize;
    for query_char in query.chars() {
        let offset = candidate_chars[search_from..]
            .iter()
            .position(|candidate_char| *candidate_char == query_char)?;
        let index = search_from + offset;
        first.get_or_insert(index);
        if let Some(previous) = previous {
            gaps += index.saturating_sub(previous + 1);
        }
        previous = Some(index);
        search_from = index + 1;
    }
    Some(
        7_000
            - first.unwrap_or_default() as i64 * 25
            - gaps as i64 * 35
            - candidate_chars.len() as i64,
    )
}

fn is_recursive_excluded_directory(name: &str) -> bool {
    RECURSIVE_EXCLUDED_DIRECTORIES
        .iter()
        .any(|excluded| name.eq_ignore_ascii_case(excluded))
}

pub(crate) fn file_icon(path: &Path) -> &'static str {
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

pub(crate) fn slash_completion_range(input: &str, cursor: usize) -> Option<(usize, usize)> {
    slash_completion_context(input, cursor).map(|context| (context.start, context.end))
}

fn is_completion_name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_project_file_completion_starts_at_the_filesystem_root() {
        let temp = tempfile::tempdir().unwrap();
        let working_directory = temp.path().canonicalize().unwrap();
        let context =
            file_completion_context_with_policy("@", 1, &working_directory, true).unwrap();

        assert_eq!(context.directory, filesystem_root(&working_directory));
        assert!(
            file_completion_context_with_policy("@relative", 9, &working_directory, true).is_none()
        );
    }

    #[test]
    fn slash_completion_combines_commands_and_contextual_skills() {
        let root = tempfile::tempdir().unwrap();
        let skills = [("review-rust", "Review Rust changes.")];

        let menu = complete("/", 1, root.path(), skills, true).unwrap();
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
        assert!(
            menu.items
                .iter()
                .any(|item| item.kind == CompletionKind::Command && item.name == "goal")
        );
        assert!(
            menu.items
                .iter()
                .any(|item| item.kind == CompletionKind::Command && item.name == "goals")
        );
        assert!(
            menu.items
                .iter()
                .any(|item| item.kind == CompletionKind::Command && item.name == "processes")
        );
        assert!(
            menu.items
                .iter()
                .any(|item| item.kind == CompletionKind::Command && item.name == "usage")
        );
        assert!(menu.items.iter().any(|item| {
            item.kind == CompletionKind::Command
                && item.name == "models"
                && item.description == "Choose model, reasoning, and speed"
        }));
        assert!(
            menu.items
                .iter()
                .any(|item| { item.kind == CompletionKind::Command && item.name == "providers" })
        );
        assert!(menu.items.iter().all(|item| {
            item.kind != CompletionKind::Command
                || (item.name != "model" && item.name != "provider")
        }));
        let without_usage = complete("/", 1, root.path(), skills, false).unwrap();
        assert!(without_usage.items.iter().all(|item| item.name != "usage"));
        assert!(
            menu.items
                .iter()
                .all(|item| item.kind != CompletionKind::Command || item.name != "clear")
        );

        let input = "Please /";
        let menu = complete(input, input.len(), root.path(), skills, true).unwrap();
        assert!(
            menu.items
                .iter()
                .all(|item| item.kind == CompletionKind::Skill)
        );
    }

    #[test]
    fn goal_objectives_are_commands_only_at_the_start_of_the_input() {
        assert_eq!(
            goal_objective_from_input("/goal Finish the migration"),
            Some("Finish the migration")
        );
        assert_eq!(
            goal_objective_from_input("/goal First line\nsecond line"),
            Some("First line\nsecond line")
        );
        assert_eq!(goal_objective_from_input("Explain /goal syntax"), None);
        assert_eq!(goal_objective_from_input("/goals"), None);
        assert_eq!(goal_objective_from_input("/goal"), None);
    }

    #[test]
    fn file_completion_uses_platform_paths_and_shared_replacements() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(workspace.join("src")).unwrap();
        fs::write(workspace.join("hello.txt"), "hello").unwrap();
        fs::write(temp.path().join("above.md"), "above").unwrap();

        let menu = complete("@", 1, &workspace, [], false).unwrap();
        let directory = menu.items.iter().find(|item| item.name == "src").unwrap();
        assert_eq!(directory.kind, CompletionKind::Directory);
        assert_eq!(directory.icon, Some(NERD_FOLDER));
        assert_eq!(directory.replacement, "@src/");
        let file = menu
            .items
            .iter()
            .find(|item| item.name == "hello.txt")
            .unwrap();
        assert_eq!(file.replacement, "@hello.txt ");

        let input = "@../abo";
        let menu = complete(input, input.len(), &workspace, [], false).unwrap();
        assert!(menu.items.iter().any(|item| item.name == "../above.md"));
    }

    #[test]
    fn local_file_completion_is_case_insensitive_and_ranks_contains_matches() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("CONFIG"), "").unwrap();
        fs::write(temp.path().join("config-old.toml"), "").unwrap();
        fs::write(temp.path().join("my-config-file.toml"), "").unwrap();
        fs::write(temp.path().join("unrelated.toml"), "").unwrap();

        let menu = complete("@config", 7, temp.path(), [], false).unwrap();
        let names = menu
            .items
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec!["CONFIG", "config-old.toml", "my-config-file.toml"]
        );
        assert_eq!(menu.items[2].display, "my-config-file.toml");
    }

    #[test]
    fn fuzzy_scoring_prefers_exact_prefix_and_substring_before_subsequence() {
        let exact = fuzzy_score("config", "config").unwrap();
        let prefix = fuzzy_score("configuration", "config").unwrap();
        let substring = fuzzy_score("my-config-file", "config").unwrap();
        let subsequence = fuzzy_score("coarse-network-file-index-generator", "config").unwrap();

        assert!(exact > prefix);
        assert!(prefix > substring);
        assert!(substring > subsequence);
    }

    #[tokio::test]
    async fn recursive_completion_finds_descendants_and_skips_generated_trees() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src/deep")).unwrap();
        fs::create_dir_all(temp.path().join("target/deep")).unwrap();
        fs::write(temp.path().join("src/deep/my-config-file.toml"), "").unwrap();
        fs::write(temp.path().join("target/deep/hidden-config.toml"), "").unwrap();
        let context = file_completion_context("@config", 7, temp.path()).unwrap();
        let local = file_completion_items(&context);
        let mut search =
            start_recursive_file_completion_with_policy(context, local, 41, test_policy()).unwrap();
        let mut items = Vec::new();

        while let Some(update) = tokio::time::timeout(Duration::from_secs(1), search.recv())
            .await
            .unwrap()
        {
            assert_eq!(update.request_id, 41);
            items = update.items;
        }

        let match_item = items
            .iter()
            .find(|item| item.name == "src/deep/my-config-file.toml")
            .unwrap();
        assert_eq!(match_item.display, "src/deep/my-config-file.toml");
        assert_eq!(match_item.replacement, "@src/deep/my-config-file.toml ");
        assert!(
            items
                .iter()
                .all(|item| !item.name.contains("hidden-config"))
        );
    }

    #[tokio::test]
    async fn recursive_completion_prioritizes_normal_paths_before_hidden_and_ignored_paths() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::create_dir_all(temp.path().join(".secret")).unwrap();
        fs::create_dir_all(temp.path().join("generated")).unwrap();
        fs::write(temp.path().join(".gitignore"), "generated/\n").unwrap();
        fs::write(temp.path().join("src/normal-match.txt"), "").unwrap();
        fs::write(temp.path().join(".secret/hidden-match.txt"), "").unwrap();
        fs::write(temp.path().join("generated/ignored-match.txt"), "").unwrap();
        let context = file_completion_context("@match", 6, temp.path()).unwrap();
        let mut search =
            start_recursive_file_completion_with_policy(context, Vec::new(), 51, test_policy())
                .unwrap();
        let mut items = Vec::new();

        while let Some(update) = tokio::time::timeout(Duration::from_secs(1), search.recv())
            .await
            .unwrap()
        {
            items = update.items;
        }

        let positions = [
            "src/normal-match.txt",
            ".secret/hidden-match.txt",
            "generated/ignored-match.txt",
        ]
        .map(|name| items.iter().position(|item| item.name == name).unwrap());
        assert!(positions[0] < positions[1]);
        assert!(positions[0] < positions[2]);
    }

    #[test]
    fn local_completion_applies_nested_gitignore_rules_and_negations() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("sub")).unwrap();
        fs::write(temp.path().join(".gitignore"), "*.tmp\nblocked/\n").unwrap();
        fs::write(temp.path().join("sub/.gitignore"), "!keep.tmp\nhide.txt\n").unwrap();
        fs::create_dir_all(temp.path().join("sub/blocked")).unwrap();
        fs::write(temp.path().join("sub/normal.txt"), "").unwrap();
        fs::write(temp.path().join("sub/keep.tmp"), "").unwrap();
        fs::write(temp.path().join("sub/drop.tmp"), "").unwrap();
        fs::write(temp.path().join("sub/hide.txt"), "").unwrap();
        fs::write(temp.path().join("sub/blocked/child.txt"), "").unwrap();

        let menu = complete("@sub/", 5, temp.path(), [], false).unwrap();
        let position = |name| {
            menu.items
                .iter()
                .position(|item| item.display == name)
                .unwrap()
        };

        assert!(position("normal.txt") < position("drop.tmp"));
        assert!(position("keep.tmp") < position("drop.tmp"));
        assert!(position("keep.tmp") < position("hide.txt"));
        let blocked = temp.path().join("sub/blocked/child.txt");
        assert!(file_priority_index(temp.path()).is_git_ignored(&blocked, false));
    }

    #[tokio::test]
    async fn recursive_completion_keeps_local_results_before_higher_priority_descendants() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join(".local-match.txt"), "").unwrap();
        fs::write(temp.path().join("src/normal-match.txt"), "").unwrap();
        let context = file_completion_context("@match", 6, temp.path()).unwrap();
        let local = file_completion_items(&context);
        let mut search =
            start_recursive_file_completion_with_policy(context, local, 52, test_policy()).unwrap();
        let mut items = Vec::new();

        while let Some(update) = tokio::time::timeout(Duration::from_secs(1), search.recv())
            .await
            .unwrap()
        {
            items = update.items;
        }

        assert_eq!(items[0].name, ".local-match.txt");
        assert!(items.iter().any(|item| item.name == "src/normal-match.txt"));
    }

    #[tokio::test]
    async fn recursive_completion_is_bounded_deduplicated_and_cancellable() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("nested")).unwrap();
        for index in 0..20 {
            fs::write(
                temp.path().join(format!("nested/config-{index:02}.toml")),
                "",
            )
            .unwrap();
        }
        let context = file_completion_context("@config", 7, temp.path()).unwrap();
        let mut policy = test_policy();
        policy.maximum_results = 3;
        let mut search =
            start_recursive_file_completion_with_policy(context, Vec::new(), 9, policy).unwrap();
        let cancelled = search.cancelled.clone();
        let mut last = Vec::new();
        while let Some(update) = tokio::time::timeout(Duration::from_secs(1), search.recv())
            .await
            .unwrap()
        {
            last = update.items;
        }
        assert!(last.len() <= 3);
        assert_eq!(
            last.iter()
                .map(|item| item.id.as_str())
                .collect::<HashSet<_>>()
                .len(),
            last.len()
        );

        drop(search);
        assert!(cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn absolute_search_roots_keep_complete_platform_replacements() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("absolute-file.txt"), "").unwrap();
        let display_root = temp.path().to_string_lossy().replace('\\', "/");
        let input = format!("@{display_root}/absolute");
        let menu = complete(&input, input.len(), Path::new("."), [], false).unwrap();
        let item = menu
            .items
            .iter()
            .find(|item| item.display == "absolute-file.txt")
            .unwrap();

        assert_eq!(
            item.replacement,
            format!("@{display_root}/absolute-file.txt ")
        );
    }

    fn test_policy() -> RecursiveSearchPolicy {
        RecursiveSearchPolicy {
            minimum_query_characters: 2,
            maximum_local_entries: 100,
            maximum_local_results: 8,
            maximum_results: 8,
            maximum_visited_entries: 200,
            maximum_visited_directories: 20,
            maximum_depth: 5,
            maximum_elapsed: Duration::from_secs(1),
            entries_per_batch: 1,
            update_interval: Duration::ZERO,
            debounce: Duration::ZERO,
            maximum_concurrent_searches: 2,
        }
    }
}
