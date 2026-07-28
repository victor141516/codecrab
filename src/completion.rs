use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;

pub(crate) const COMMANDS: &[(&str, &str)] = &[
    ("help", "Open keyboard and command help"),
    ("model", "Choose model, reasoning, and speed"),
    ("models", "Alias for /model"),
    ("skills", "Open the interactive skill picker"),
    ("sessions", "Browse, resume, or delete saved sessions"),
    ("goal", "Start a persistent goal"),
    ("goals", "Browse and manage persistent goals"),
    ("clear", "Clear the conversation context"),
    ("quit", "Save the session and exit"),
];

pub(crate) const NERD_FOLDER: &str = "";
const NERD_FILE: &str = "";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionKind {
    Command,
    Skill,
    File,
    Directory,
}

#[derive(Clone, Serialize)]
pub(crate) struct CompletionItem {
    pub(crate) name: String,
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

pub(crate) fn complete<'a>(
    input: &str,
    cursor: usize,
    working_directory: &Path,
    skills: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<CompletionMenu> {
    if let Some(context) = file_completion_context(input, cursor, working_directory) {
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
                .filter(|(name, _)| name.starts_with(context.prefix))
                .map(|(name, description)| CompletionItem {
                    name: (*name).to_owned(),
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
                name: name.to_owned(),
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

pub(crate) struct FileCompletionContext {
    pub(crate) start: usize,
    pub(crate) end: usize,
    dir_prefix: String,
    name_prefix: String,
    pub(crate) directory: PathBuf,
}

pub(crate) fn file_completion_context(
    input: &str,
    cursor: usize,
    working_directory: &Path,
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
            let name = format!("{}{}", context.dir_prefix, name);
            let suffix = if is_dir { "/" } else { " " };
            Some(CompletionItem {
                replacement: format!("@{name}{suffix}"),
                name,
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

fn is_completion_name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_completion_combines_commands_and_contextual_skills() {
        let root = tempfile::tempdir().unwrap();
        let skills = [("review-rust", "Review Rust changes.")];

        let menu = complete("/", 1, root.path(), skills).unwrap();
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

        let input = "Please /";
        let menu = complete(input, input.len(), root.path(), skills).unwrap();
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

        let menu = complete("@", 1, &workspace, []).unwrap();
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
        let menu = complete(input, input.len(), &workspace, []).unwrap();
        assert!(menu.items.iter().any(|item| item.name == "../above.md"));
    }
}
