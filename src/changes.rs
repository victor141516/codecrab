use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use diffy::{Patch, apply, create_patch};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    session::{FileChange, FileChangeKind, FileChangeSet, GitFileChange, TemporaryFileChange},
    tools::ToolBox,
};

#[derive(Clone)]
pub(crate) struct ChangeStore {
    project_root: Option<PathBuf>,
    data_dir: PathBuf,
    session_id: Uuid,
}

#[derive(Clone, Copy)]
struct GitProject;

#[derive(Clone)]
struct TurnFile {
    path: PathBuf,
    before: Option<String>,
    git: Option<GitAnchor>,
    temporary_before: Option<PathBuf>,
    capture_error: Option<String>,
    effective_change: bool,
}

#[derive(Clone)]
struct GitAnchor {
    commit: String,
    relative_path: PathBuf,
    base: String,
}

pub(crate) struct ChangeTracker {
    store: ChangeStore,
    git_project: Option<GitProject>,
    turn_message_id: Option<Uuid>,
    turn_files: HashMap<PathBuf, TurnFile>,
}

pub(crate) struct PendingChange {
    path: PathBuf,
    before: Option<String>,
    git: Option<GitAnchor>,
    capture_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ReconstructedChangeSet {
    pub id: Uuid,
    pub kind: FileChangeKind,
    pub files: Vec<ReconstructedFileChange>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ReconstructedFileChange {
    pub path: PathBuf,
    pub before: String,
    pub after: String,
    pub changed_lines: usize,
    pub focus_line: usize,
}

impl ChangeStore {
    pub(crate) fn new(project_root: &Path, session_id: Uuid) -> Self {
        Self {
            project_root: Some(project_root.to_path_buf()),
            data_dir: project_root.join(".codecrab").join("session-data"),
            session_id,
        }
    }

    pub(crate) fn no_project_at(data_root: &Path, session_id: Uuid) -> Result<Self> {
        Ok(Self {
            project_root: None,
            data_dir: data_root.join("session-data"),
            session_id,
        })
    }

    pub(crate) fn session_dir(&self) -> PathBuf {
        self.data_dir.join(self.session_id.to_string())
    }

    fn write_artifact(&self, change_id: Uuid, name: &str, content: &str) -> Result<PathBuf> {
        let relative = PathBuf::from("changes")
            .join(change_id.to_string())
            .join(name);
        let path = self.session_dir().join(&relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        fs::write(&path, content).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(relative)
    }

    fn read_artifact(&self, relative: &Path) -> Result<String> {
        let path = self.session_dir().join(relative);
        fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))
    }

    fn persist_change(&self, change: &FileChangeSet) -> Result<()> {
        let relative = PathBuf::from("changes")
            .join(change.id.to_string())
            .join("change.json");
        let path = self.session_dir().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        fs::write(&path, serde_json::to_vec_pretty(change)?)
            .with_context(|| format!("cannot write {}", path.display()))
    }

    pub(crate) fn load_change(&self, id: Uuid) -> Result<FileChangeSet> {
        let path = self
            .session_dir()
            .join("changes")
            .join(id.to_string())
            .join("change.json");
        let bytes = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_slice(&bytes).context("invalid persisted file change")
    }

    pub(crate) fn reconstruct(&self, change: &FileChangeSet) -> Result<ReconstructedChangeSet> {
        if let Some(reason) = &change.unavailable_reason {
            anyhow::bail!(reason.clone());
        }
        let mut files = Vec::with_capacity(change.files.len());
        for file in &change.files {
            let (before, after) = match file {
                FileChange::Git(git) => self.reconstruct_git(git)?,
                FileChange::Temporary(temporary) => (
                    self.read_artifact(&temporary.before)?,
                    self.read_artifact(&temporary.after)?,
                ),
            };
            let (changed_lines, focus_line) = diff_stats(&before, &after);
            files.push(ReconstructedFileChange {
                path: file.path().to_path_buf(),
                before,
                after,
                changed_lines,
                focus_line,
            });
        }
        files.sort_by_key(|file| std::cmp::Reverse(file.changed_lines));
        Ok(ReconstructedChangeSet {
            id: change.id,
            kind: change.kind,
            files,
        })
    }

    fn reconstruct_git(&self, change: &GitFileChange) -> Result<(String, String)> {
        let project_root = self
            .project_root
            .as_ref()
            .context("Git reconstruction requires a project")?;
        let output = Command::new("git")
            .args(["-C", project_root.to_string_lossy().as_ref(), "show"])
            .arg(format!(
                "{}:{}",
                change.commit,
                change.relative_path.to_string_lossy().replace('\\', "/")
            ))
            .output()
            .context("cannot run git show")?;
        if !output.status.success() {
            anyhow::bail!(
                "The commit used as the base for this change is no longer available, so this diff cannot be reconstructed."
            );
        }
        let base = String::from_utf8(output.stdout).context("Git blob is not UTF-8 text")?;
        let before_patch_text = self.read_artifact(&change.before_patch)?;
        let after_patch_text = self.read_artifact(&change.after_patch)?;
        let before_patch = Patch::from_str(&before_patch_text).context("invalid before patch")?;
        let after_patch = Patch::from_str(&after_patch_text).context("invalid after patch")?;
        let before =
            apply(&base, &before_patch).context("cannot reconstruct pre-change content")?;
        verify_hash(&before, &change.before_sha256)?;
        let after = apply(&before, &after_patch).context("cannot reconstruct changed content")?;
        verify_hash(&after, &change.after_sha256)?;
        Ok((before, after))
    }
}

impl ChangeTracker {
    pub(crate) fn new(project_root: &Path, session_id: Uuid) -> Self {
        Self {
            store: ChangeStore::new(project_root, session_id),
            git_project: detect_git_project(project_root),
            turn_message_id: None,
            turn_files: HashMap::new(),
        }
    }

    pub(crate) fn no_project_at(data_root: &Path, session_id: Uuid) -> Result<Self> {
        Ok(Self {
            store: ChangeStore::no_project_at(data_root, session_id)?,
            git_project: None,
            turn_message_id: None,
            turn_files: HashMap::new(),
        })
    }

    pub(crate) fn begin_turn(&mut self, turn_message_id: Uuid, previous_changes: &[FileChangeSet]) {
        for change in previous_changes {
            for file in &change.files {
                if let FileChange::Temporary(temporary) = file {
                    let _ = fs::remove_file(self.store.session_dir().join(&temporary.before));
                    let _ = fs::remove_file(self.store.session_dir().join(&temporary.after));
                }
            }
        }
        self.turn_message_id = Some(turn_message_id);
        self.turn_files.clear();
    }

    pub(crate) fn before_operation(
        &mut self,
        tools: &ToolBox,
        tool: &str,
        arguments: &str,
    ) -> Result<Option<PendingChange>> {
        if !matches!(tool, "write_file" | "replace_in_file") {
            return Ok(None);
        }
        let path = tools.resolve_tool_file_path(tool, arguments)?;
        let (before, mut capture_error) = match read_optional_text(&path) {
            Ok(before) => (before, None),
            Err(error) => (
                None,
                Some(format!("cannot capture {}: {error:#}", path.display())),
            ),
        };
        let git = if capture_error.is_none() {
            match self.git_anchor(&path) {
                Ok(git) => git,
                Err(error) => {
                    capture_error = Some(format!(
                        "cannot inspect Git history for {}: {error:#}",
                        path.display()
                    ));
                    None
                }
            }
        } else {
            None
        };
        self.turn_files
            .entry(path.clone())
            .or_insert_with(|| TurnFile {
                path: path.clone(),
                before: before.clone(),
                git: git.clone(),
                temporary_before: None,
                capture_error: capture_error.clone(),
                effective_change: false,
            });
        Ok(Some(PendingChange {
            path,
            before,
            git,
            capture_error,
        }))
    }

    pub(crate) fn after_operation(
        &mut self,
        activity_id: &str,
        turn_message_id: Uuid,
        pending: PendingChange,
        tool_succeeded: bool,
    ) -> Result<Option<FileChangeSet>> {
        let after = read_optional_text(&pending.path);
        let unavailable_reason = pending.capture_error.or_else(|| {
            after.as_ref().err().map(|error| {
                format!(
                    "cannot capture {} after the edit: {error:#}",
                    pending.path.display()
                )
            })
        });
        if unavailable_reason.is_some() {
            if !tool_succeeded {
                return Ok(None);
            }
            if let Some(turn_file) = self.turn_files.get_mut(&pending.path) {
                turn_file.effective_change = true;
                turn_file.capture_error = unavailable_reason.clone();
            }
            let change = FileChangeSet {
                id: Uuid::new_v4(),
                turn_message_id,
                activity_id: Some(activity_id.to_owned()),
                kind: FileChangeKind::Operation,
                outcome: None,
                unavailable_reason,
                files: Vec::new(),
            };
            self.store.persist_change(&change)?;
            return Ok(Some(change));
        }
        let after = after.expect("successful snapshot checked above");
        if pending.before == after {
            return Ok(None);
        }
        if let Some(turn_file) = self.turn_files.get_mut(&pending.path) {
            turn_file.effective_change = true;
        }
        let before = pending.before.unwrap_or_default();
        let after = after.unwrap_or_default();
        let id = Uuid::new_v4();
        let file = if let Some(git) = pending.git {
            let before_patch = create_patch(&git.base, &before).to_string();
            let after_patch = create_patch(&before, &after).to_string();
            let before_patch = self
                .store
                .write_artifact(id, "before.patch", &before_patch)?;
            let after_patch = self.store.write_artifact(id, "after.patch", &after_patch)?;
            FileChange::Git(GitFileChange {
                path: pending.path,
                commit: git.commit,
                relative_path: git.relative_path,
                before_patch,
                after_patch,
                before_sha256: sha256(&before),
                after_sha256: sha256(&after),
            })
        } else {
            let turn_file = self
                .turn_files
                .get_mut(&pending.path)
                .context("tracked turn file disappeared")?;
            let before_path = match &turn_file.temporary_before {
                Some(path) => path.clone(),
                None => {
                    let path = self.store.write_artifact(
                        id,
                        "before.txt",
                        turn_file.before.as_deref().unwrap_or_default(),
                    )?;
                    turn_file.temporary_before = Some(path.clone());
                    path
                }
            };
            let after_path = self.store.write_artifact(id, "after.txt", &after)?;
            FileChange::Temporary(TemporaryFileChange {
                path: pending.path,
                before: before_path,
                after: after_path,
            })
        };
        let change = FileChangeSet {
            id,
            turn_message_id,
            activity_id: Some(activity_id.to_owned()),
            kind: FileChangeKind::Operation,
            outcome: None,
            unavailable_reason: None,
            files: vec![file],
        };
        self.store.persist_change(&change)?;
        Ok(Some(change))
    }

    pub(crate) fn finish_turn(
        &mut self,
        outcome: crate::session::TurnOutcome,
    ) -> Result<Option<FileChangeSet>> {
        let Some(turn_message_id) = self.turn_message_id.take() else {
            return Ok(None);
        };
        let files = std::mem::take(&mut self.turn_files);
        let id = Uuid::new_v4();
        let mut changes = Vec::new();
        let mut unavailable_reason = None;
        for (_, turn_file) in files {
            if turn_file.effective_change && turn_file.capture_error.is_some() {
                unavailable_reason = turn_file.capture_error;
                continue;
            }
            let after = match read_optional_text(&turn_file.path) {
                Ok(after) => after,
                Err(error) if turn_file.effective_change => {
                    unavailable_reason = Some(format!(
                        "cannot capture {} at the end of the turn: {error:#}",
                        turn_file.path.display()
                    ));
                    continue;
                }
                Err(error) => return Err(error),
            };
            if turn_file.before == after {
                continue;
            }
            let before = turn_file.before.unwrap_or_default();
            let after = after.unwrap_or_default();
            if let Some(git) = turn_file.git {
                let before_patch = self.store.write_artifact(
                    id,
                    &format!("{}-before.patch", changes.len()),
                    &create_patch(&git.base, &before).to_string(),
                )?;
                let after_patch = self.store.write_artifact(
                    id,
                    &format!("{}-after.patch", changes.len()),
                    &create_patch(&before, &after).to_string(),
                )?;
                changes.push(FileChange::Git(GitFileChange {
                    path: turn_file.path,
                    commit: git.commit,
                    relative_path: git.relative_path,
                    before_patch,
                    after_patch,
                    before_sha256: sha256(&before),
                    after_sha256: sha256(&after),
                }));
            } else {
                let before_path = match turn_file.temporary_before {
                    Some(path) => path,
                    None => self.store.write_artifact(
                        id,
                        &format!("{}-before.txt", changes.len()),
                        &before,
                    )?,
                };
                let after_path = self.store.write_artifact(
                    id,
                    &format!("{}-after.txt", changes.len()),
                    &after,
                )?;
                changes.push(FileChange::Temporary(TemporaryFileChange {
                    path: turn_file.path,
                    before: before_path,
                    after: after_path,
                }));
            }
        }
        if changes.is_empty() && unavailable_reason.is_none() {
            return Ok(None);
        }
        let change = FileChangeSet {
            id,
            turn_message_id,
            activity_id: None,
            kind: FileChangeKind::Turn,
            outcome: Some(outcome),
            unavailable_reason,
            files: changes,
        };
        self.store.persist_change(&change)?;
        Ok(Some(change))
    }

    fn git_anchor(&self, path: &Path) -> Result<Option<GitAnchor>> {
        let Some(_) = &self.git_project else {
            return Ok(None);
        };
        let Some(project_root) = self.store.project_root.as_ref() else {
            return Ok(None);
        };
        let Ok(relative_path) = path.strip_prefix(project_root) else {
            return Ok(None);
        };
        let relative = relative_path.to_string_lossy().replace('\\', "/");
        let ignored = Command::new("git")
            .args([
                "-C",
                project_root.to_string_lossy().as_ref(),
                "check-ignore",
                "--no-index",
                "--quiet",
                "--",
                &relative,
            ])
            .status()
            .context("cannot inspect Git ignore rules")?
            .success();
        if ignored {
            return Ok(None);
        }
        let head_output = Command::new("git")
            .args([
                "-C",
                project_root.to_string_lossy().as_ref(),
                "rev-parse",
                "HEAD",
            ])
            .output()
            .context("cannot inspect Git HEAD")?;
        if !head_output.status.success() {
            return Ok(None);
        }
        let head = String::from_utf8(head_output.stdout)
            .context("Git HEAD is not UTF-8")?
            .trim()
            .to_owned();
        let exists = Command::new("git")
            .args([
                "-C",
                project_root.to_string_lossy().as_ref(),
                "cat-file",
                "-e",
                &format!("{}:{relative}", head),
            ])
            .output()
            .context("cannot inspect Git file")?
            .status
            .success();
        if !exists {
            return Ok(None);
        }
        let output = Command::new("git")
            .args([
                "-C",
                project_root.to_string_lossy().as_ref(),
                "show",
                &format!("{}:{relative}", head),
            ])
            .output()
            .context("cannot read Git file")?;
        if !output.status.success() {
            return Ok(None);
        }
        let base = String::from_utf8(output.stdout).context("Git blob is not UTF-8 text")?;
        Ok(Some(GitAnchor {
            commit: head.clone(),
            relative_path: relative_path.to_path_buf(),
            base,
        }))
    }
}

fn detect_git_project(project_root: &Path) -> Option<GitProject> {
    let top = Command::new("git")
        .args([
            "-C",
            project_root.to_string_lossy().as_ref(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output()
        .ok()?;
    if !top.status.success() {
        return None;
    }
    let top = PathBuf::from(String::from_utf8(top.stdout).ok()?.trim());
    let canonical_top = fs::canonicalize(top).ok()?;
    let canonical_root = fs::canonicalize(project_root).ok()?;
    if canonical_top != canonical_root {
        return None;
    }
    let head = Command::new("git")
        .args([
            "-C",
            project_root.to_string_lossy().as_ref(),
            "rev-parse",
            "HEAD",
        ])
        .output()
        .ok()?;
    if !head.status.success() {
        return None;
    }
    Some(GitProject)
}

pub(crate) fn git_is_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn read_optional_text(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("cannot snapshot {}", path.display())),
    }
}

fn sha256(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

fn verify_hash(content: &str, expected: &str) -> Result<()> {
    if sha256(content) != expected {
        anyhow::bail!("reconstructed file hash does not match recorded change");
    }
    Ok(())
}

fn diff_stats(before: &str, after: &str) -> (usize, usize) {
    let patch = create_patch(before, after);
    let mut changed = 0;
    let mut focus_line = 1;
    let mut largest = 0;
    for hunk in patch.hunks() {
        let mut new_line = hunk.new_range().start().max(1);
        let mut run_start = new_line;
        let mut run_size = 0;
        let consider_run = |run_size: &mut usize,
                            run_start: usize,
                            largest: &mut usize,
                            focus_line: &mut usize| {
            if *run_size > *largest {
                *largest = *run_size;
                *focus_line = run_start;
            }
            *run_size = 0;
        };
        for line in hunk.lines() {
            match line {
                diffy::Line::Context(_) => {
                    consider_run(&mut run_size, run_start, &mut largest, &mut focus_line);
                    new_line += 1;
                }
                diffy::Line::Insert(_) => {
                    if run_size == 0 {
                        run_start = new_line;
                    }
                    run_size += 1;
                    changed += 1;
                    new_line += 1;
                }
                diffy::Line::Delete(_) => {
                    if run_size == 0 {
                        run_start = new_line;
                    }
                    run_size += 1;
                    changed += 1;
                }
            }
        }
        consider_run(&mut run_size, run_start, &mut largest, &mut focus_line);
    }
    (changed, focus_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_stats_prefers_the_largest_change_hunk() {
        let before = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
        let after = "ONE\ntwo\nthree\nfour\nFIVE\nSIX\nSEVEN\neight\n";
        let (changed, focus) = diff_stats(before, after);
        assert!(changed >= 4);
        assert!(focus >= 4);
    }

    #[test]
    fn git_operation_reconstructs_dirty_before_and_exact_after() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "CodeCrab Test"]);
        fs::write(temp.path().join("file.txt"), "base\n").unwrap();
        git(temp.path(), &["add", "file.txt"]);
        git(temp.path(), &["commit", "-m", "base"]);
        fs::write(temp.path().join("file.txt"), "dirty\n").unwrap();

        let session_id = Uuid::new_v4();
        let mut tracker = ChangeTracker::new(temp.path(), session_id);
        tracker.begin_turn(Uuid::new_v4(), &[]);
        let tools = ToolBox::new(temp.path().to_path_buf());
        let pending = tracker
            .before_operation(
                &tools,
                "write_file",
                r#"{"path":"file.txt","content":"after"}"#,
            )
            .unwrap()
            .unwrap();
        fs::write(temp.path().join("file.txt"), "after\n").unwrap();
        let change = tracker
            .after_operation("call-1", Uuid::new_v4(), pending, true)
            .unwrap()
            .unwrap();
        let reconstructed = tracker.store.reconstruct(&change).unwrap();
        assert_eq!(reconstructed.files[0].before, "dirty\n");
        assert_eq!(reconstructed.files[0].after, "after\n");
    }

    #[test]
    fn failed_tools_record_only_real_filesystem_effects() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.txt");
        fs::write(&path, "before\n").unwrap();
        let turn_id = Uuid::new_v4();
        let mut tracker = ChangeTracker::new(temp.path(), Uuid::new_v4());
        tracker.begin_turn(turn_id, &[]);
        let tools = ToolBox::new(temp.path().to_path_buf());

        let unchanged = tracker
            .before_operation(
                &tools,
                "write_file",
                r#"{"path":"file.txt","content":"before"}"#,
            )
            .unwrap()
            .unwrap();
        assert!(
            tracker
                .after_operation("call-1", turn_id, unchanged, false)
                .unwrap()
                .is_none()
        );

        let changed = tracker
            .before_operation(
                &tools,
                "write_file",
                r#"{"path":"file.txt","content":"partial"}"#,
            )
            .unwrap()
            .unwrap();
        fs::write(&path, "partial\n").unwrap();
        let change = tracker
            .after_operation("call-2", turn_id, changed, false)
            .unwrap()
            .unwrap();
        let reconstructed = tracker.store.reconstruct(&change).unwrap();
        assert_eq!(reconstructed.files[0].before, "before\n");
        assert_eq!(reconstructed.files[0].after, "partial\n");
    }

    #[test]
    fn cancelled_turn_includes_the_real_terminal_file_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.txt");
        fs::write(&path, "before\n").unwrap();
        let turn_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let mut tracker = ChangeTracker::new(temp.path(), session_id);
        tracker.begin_turn(turn_id, &[]);
        let tools = ToolBox::new(temp.path().to_path_buf());
        let pending = tracker
            .before_operation(
                &tools,
                "write_file",
                r#"{"path":"file.txt","content":"agent"}"#,
            )
            .unwrap()
            .unwrap();
        fs::write(&path, "agent\n").unwrap();
        tracker
            .after_operation("call-1", turn_id, pending, true)
            .unwrap();
        fs::write(&path, "concurrent\n").unwrap();

        let change = tracker
            .finish_turn(crate::session::TurnOutcome::Cancelled)
            .unwrap()
            .unwrap();
        assert_eq!(change.outcome, Some(crate::session::TurnOutcome::Cancelled));
        let persisted = ChangeStore::new(temp.path(), session_id)
            .load_change(change.id)
            .unwrap();
        let reconstructed = tracker.store.reconstruct(&persisted).unwrap();
        assert_eq!(reconstructed.files[0].before, "before\n");
        assert_eq!(reconstructed.files[0].after, "concurrent\n");
    }

    #[test]
    fn unavailable_git_bases_keep_the_required_explanation() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "CodeCrab Test"]);
        let path = temp.path().join("file.txt");
        fs::write(&path, "base\n").unwrap();
        git(temp.path(), &["add", "file.txt"]);
        git(temp.path(), &["commit", "-m", "base"]);
        let turn_id = Uuid::new_v4();
        let mut tracker = ChangeTracker::new(temp.path(), Uuid::new_v4());
        tracker.begin_turn(turn_id, &[]);
        let tools = ToolBox::new(temp.path().to_path_buf());
        let pending = tracker
            .before_operation(
                &tools,
                "write_file",
                r#"{"path":"file.txt","content":"after"}"#,
            )
            .unwrap()
            .unwrap();
        fs::write(&path, "after\n").unwrap();
        let mut change = tracker
            .after_operation("call-1", turn_id, pending, true)
            .unwrap()
            .unwrap();
        let FileChange::Git(file) = &mut change.files[0] else {
            panic!("tracked file unexpectedly used temporary storage");
        };
        file.commit = "0000000000000000000000000000000000000000".into();

        let error = tracker.store.reconstruct(&change).unwrap_err();
        assert_eq!(
            error.to_string(),
            "The commit used as the base for this change is no longer available, so this diff cannot be reconstructed."
        );
    }

    #[test]
    fn repeated_git_edits_keep_exact_operations_and_one_accumulated_turn() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "CodeCrab Test"]);
        let path = temp.path().join("file.txt");
        fs::write(&path, "base\n").unwrap();
        git(temp.path(), &["add", "file.txt"]);
        git(temp.path(), &["commit", "-m", "base"]);
        fs::write(&path, "dirty\n").unwrap();

        let mut tracker = ChangeTracker::new(temp.path(), Uuid::new_v4());
        let turn_id = Uuid::new_v4();
        tracker.begin_turn(turn_id, &[]);
        let tools = ToolBox::new(temp.path().to_path_buf());
        let first = tracker
            .before_operation(
                &tools,
                "write_file",
                r#"{"path":"file.txt","content":"one"}"#,
            )
            .unwrap()
            .unwrap();
        fs::write(&path, "one\n").unwrap();
        let first = tracker
            .after_operation("call-1", turn_id, first, true)
            .unwrap()
            .unwrap();

        git(temp.path(), &["add", "file.txt"]);
        git(temp.path(), &["commit", "-m", "mid-turn"]);
        let second = tracker
            .before_operation(
                &tools,
                "write_file",
                r#"{"path":"file.txt","content":"two"}"#,
            )
            .unwrap()
            .unwrap();
        fs::write(&path, "two\n").unwrap();
        let second = tracker
            .after_operation("call-2", turn_id, second, true)
            .unwrap()
            .unwrap();
        let turn = tracker
            .finish_turn(crate::session::TurnOutcome::Completed)
            .unwrap()
            .unwrap();

        let first = tracker.store.reconstruct(&first).unwrap();
        let second = tracker.store.reconstruct(&second).unwrap();
        let turn = tracker.store.reconstruct(&turn).unwrap();
        assert_eq!(
            (
                first.files[0].before.as_str(),
                first.files[0].after.as_str()
            ),
            ("dirty\n", "one\n")
        );
        assert_eq!(
            (
                second.files[0].before.as_str(),
                second.files[0].after.as_str()
            ),
            ("one\n", "two\n")
        );
        assert_eq!(
            (turn.files[0].before.as_str(), turn.files[0].after.as_str()),
            ("dirty\n", "two\n")
        );
    }

    #[test]
    fn non_git_turn_is_cumulative_and_purged_at_the_next_turn() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.txt");
        fs::write(&path, "before\n").unwrap();
        let session_id = Uuid::new_v4();
        let mut tracker = ChangeTracker::new(temp.path(), session_id);
        tracker.begin_turn(Uuid::new_v4(), &[]);
        let tools = ToolBox::new(temp.path().to_path_buf());
        let first = tracker
            .before_operation(
                &tools,
                "write_file",
                r#"{"path":"file.txt","content":"one"}"#,
            )
            .unwrap()
            .unwrap();
        fs::write(&path, "one\n").unwrap();
        let first_change = tracker
            .after_operation("call-1", Uuid::new_v4(), first, true)
            .unwrap()
            .unwrap();
        let second = tracker
            .before_operation(
                &tools,
                "write_file",
                r#"{"path":"file.txt","content":"two"}"#,
            )
            .unwrap()
            .unwrap();
        fs::write(&path, "two\n").unwrap();
        let second_change = tracker
            .after_operation("call-2", Uuid::new_v4(), second, true)
            .unwrap()
            .unwrap();
        let turn = tracker
            .finish_turn(crate::session::TurnOutcome::Completed)
            .unwrap()
            .unwrap();
        let temporary_before = |change: &FileChangeSet| match &change.files[0] {
            FileChange::Temporary(file) => file.before.clone(),
            FileChange::Git(_) => panic!("non-Git change unexpectedly used Git storage"),
        };
        assert_eq!(
            temporary_before(&first_change),
            temporary_before(&second_change)
        );
        assert_eq!(temporary_before(&first_change), temporary_before(&turn));
        let reconstructed = tracker.store.reconstruct(&turn).unwrap();
        assert_eq!(reconstructed.files[0].before, "before\n");
        assert_eq!(reconstructed.files[0].after, "two\n");
        tracker.begin_turn(Uuid::new_v4(), std::slice::from_ref(&turn));
        assert!(tracker.store.reconstruct(&turn).is_err());
    }

    #[test]
    fn ignored_tracked_files_use_temporary_capture() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "CodeCrab Test"]);
        fs::write(temp.path().join("ignored.txt"), "base\n").unwrap();
        git(temp.path(), &["add", "ignored.txt"]);
        git(temp.path(), &["commit", "-m", "base"]);
        fs::write(temp.path().join(".gitignore"), "ignored.txt\n").unwrap();

        let mut tracker = ChangeTracker::new(temp.path(), Uuid::new_v4());
        tracker.begin_turn(Uuid::new_v4(), &[]);
        let tools = ToolBox::new(temp.path().to_path_buf());
        let pending = tracker
            .before_operation(
                &tools,
                "write_file",
                r#"{"path":"ignored.txt","content":"after"}"#,
            )
            .unwrap()
            .unwrap();

        assert!(pending.git.is_none());
    }

    #[test]
    fn subdirectories_new_files_and_external_paths_do_not_borrow_other_git_history() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init"]);
        git(root.path(), &["config", "user.email", "test@example.com"]);
        git(root.path(), &["config", "user.name", "CodeCrab Test"]);
        fs::create_dir_all(root.path().join("subproject")).unwrap();
        fs::write(root.path().join("tracked.txt"), "base\n").unwrap();
        git(root.path(), &["add", "tracked.txt"]);
        git(root.path(), &["commit", "-m", "base"]);

        let subproject = root.path().join("subproject");
        let subproject_tracker = ChangeTracker::new(&subproject, Uuid::new_v4());
        assert!(subproject_tracker.git_project.is_none());

        let mut tracker = ChangeTracker::new(root.path(), Uuid::new_v4());
        tracker.begin_turn(Uuid::new_v4(), &[]);
        let tools = ToolBox::new(root.path().to_path_buf());
        let new_file = tracker
            .before_operation(
                &tools,
                "write_file",
                r#"{"path":"new.txt","content":"new"}"#,
            )
            .unwrap()
            .unwrap();
        assert!(new_file.git.is_none());

        let external = tempfile::NamedTempFile::new().unwrap();
        let arguments = serde_json::json!({
            "path": external.path(),
            "content": "outside"
        })
        .to_string();
        let external = tracker
            .before_operation(&tools, "write_file", &arguments)
            .unwrap()
            .unwrap();
        assert!(external.git.is_none());
    }

    #[test]
    fn unavailable_records_keep_an_actionable_reason() {
        let temp = tempfile::tempdir().unwrap();
        let store = ChangeStore::new(temp.path(), Uuid::new_v4());
        let change = FileChangeSet {
            id: Uuid::new_v4(),
            turn_message_id: Uuid::new_v4(),
            activity_id: Some("call-1".into()),
            kind: FileChangeKind::Operation,
            outcome: None,
            unavailable_reason: Some("snapshot failed".into()),
            files: Vec::new(),
        };

        assert_eq!(
            store.reconstruct(&change).unwrap_err().to_string(),
            "snapshot failed"
        );
        store.persist_change(&change).unwrap();
        let loaded = store.load_change(change.id).unwrap();
        assert_eq!(
            loaded.unavailable_reason.as_deref(),
            Some("snapshot failed")
        );
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {} failed", args.join(" "));
    }
}
