use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::normalized_root;

#[derive(Debug, Serialize)]
pub(crate) struct DirectoryEntry {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Serialize)]
pub(crate) struct DirectoryListing {
    pub path: PathBuf,
    pub parent: Option<PathBuf>,
    pub directories: Vec<DirectoryEntry>,
    pub roots: Vec<PathBuf>,
}

pub(crate) fn browse_directories(
    base: &Path,
    requested: Option<&Path>,
) -> Result<DirectoryListing> {
    let path = resolve_path(base, requested.unwrap_or(base));
    let path = fs::canonicalize(&path)
        .with_context(|| format!("cannot open directory {}", path.display()))?;
    if !path.is_dir() {
        anyhow::bail!("{} is not a directory", path.display());
    }

    let mut directories = Vec::new();
    for entry in
        fs::read_dir(&path).with_context(|| format!("cannot list directory {}", path.display()))?
    {
        let entry = entry.with_context(|| format!("cannot read an entry in {}", path.display()))?;
        let metadata = fs::metadata(entry.path())
            .with_context(|| format!("cannot inspect {}", entry.path().display()))?;
        if metadata.is_dir() {
            directories.push(DirectoryEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: normalized_root(&entry.path()),
            });
        }
    }
    directories.sort_by_cached_key(|entry| entry.name.to_lowercase());

    Ok(DirectoryListing {
        parent: path.parent().map(normalized_root),
        roots: filesystem_roots(&path),
        path: normalized_root(&path),
        directories,
    })
}

pub(crate) fn create_directory(base: &Path, parent: &Path, name: &str) -> Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("directory name is required");
    }
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        anyhow::bail!("directory name must be one name without path separators");
    }

    let parent = resolve_path(base, parent);
    let parent = fs::canonicalize(&parent)
        .with_context(|| format!("cannot open directory {}", parent.display()))?;
    if !parent.is_dir() {
        anyhow::bail!("{} is not a directory", parent.display());
    }
    let created = parent.join(name);
    fs::create_dir(&created)
        .with_context(|| format!("cannot create directory {}", created.display()))?;
    Ok(normalized_root(&created))
}

pub(crate) fn existing_directory(base: &Path, requested: &Path) -> Result<PathBuf> {
    let path = resolve_path(base, requested);
    let path = fs::canonicalize(&path)
        .with_context(|| format!("cannot open project {}", path.display()))?;
    if !path.is_dir() {
        anyhow::bail!("{} is not a directory", path.display());
    }
    Ok(normalized_root(&path))
}

fn resolve_path(base: &Path, requested: &Path) -> PathBuf {
    if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        base.join(requested)
    }
}

#[cfg(windows)]
fn filesystem_roots(current: &Path) -> Vec<PathBuf> {
    let mut roots = (b'A'..=b'Z')
        .map(|drive| PathBuf::from(format!("{}:\\", drive as char)))
        .filter(|root| root.is_dir())
        .collect::<Vec<_>>();
    if roots.is_empty()
        && let Some(root) = current.components().next()
    {
        roots.push(PathBuf::from(root.as_os_str()));
    }
    roots
}

#[cfg(not(windows))]
fn filesystem_roots(_current: &Path) -> Vec<PathBuf> {
    vec![PathBuf::from("/")]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browsing_lists_only_directories_and_resolves_parent_paths() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base");
        let child = base.join("child");
        fs::create_dir_all(&child).unwrap();
        fs::write(base.join("file.txt"), "ignored").unwrap();

        let listing = browse_directories(&base, Some(Path::new("."))).unwrap();

        assert_eq!(listing.directories.len(), 1);
        assert_eq!(listing.directories[0].name, "child");
        assert_eq!(listing.directories[0].path, normalized_root(&child));
        assert_eq!(
            listing.parent,
            normalized_root(&base).parent().map(normalized_root)
        );
    }

    #[test]
    fn creating_a_directory_does_not_change_the_browsed_location() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base");
        fs::create_dir(&base).unwrap();

        let created = create_directory(&base, Path::new("."), "new project").unwrap();
        let listing = browse_directories(&base, None).unwrap();

        assert!(created.is_dir());
        assert_eq!(listing.path, normalized_root(&base));
        assert!(
            listing
                .directories
                .iter()
                .any(|entry| entry.path == created)
        );
    }

    #[test]
    fn directory_creation_rejects_nested_or_existing_names() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("existing")).unwrap();

        assert!(create_directory(temp.path(), temp.path(), "nested/child").is_err());
        assert!(create_directory(temp.path(), temp.path(), "existing").is_err());
    }

    #[test]
    fn opening_a_project_rejects_files_and_missing_paths() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file.txt"), "not a project").unwrap();

        assert!(existing_directory(temp.path(), Path::new("file.txt")).is_err());
        assert!(existing_directory(temp.path(), Path::new("missing")).is_err());
    }
}
