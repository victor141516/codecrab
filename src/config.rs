use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, Item, Value};

#[derive(Clone, Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    pub model: String,
    pub base_url: String,
    pub auth: String,
    pub api_key_env: String,
    pub request_timeout_seconds: u64,
    pub session_directories: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "auto".into(),
            base_url: "https://api.openai.com/v1".into(),
            auth: "auto".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            request_timeout_seconds: 180,
            session_directories: Vec::new(),
        }
    }
}
#[derive(Serialize)]
pub(crate) struct PublicConfig<'a> {
    pub model: &'a str,
    pub base_url: &'a str,
    pub auth: &'a str,
    pub api_key_env: &'a str,
    pub request_timeout_seconds: u64,
    pub session_directories: &'a [PathBuf],
}

#[derive(Clone)]
pub(crate) struct SessionRegistry {
    path: Option<PathBuf>,
}

impl Config {
    pub(crate) fn file_path() -> Option<PathBuf> {
        ProjectDirs::from("", "", "codecrab").map(|dirs| dirs.config_dir().join("config.toml"))
    }

    pub(crate) fn load() -> Result<Self> {
        let mut config = Self::default();
        if let Some(path) = Self::file_path()
            && path.exists()
        {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            config = toml::from_str(&text)
                .with_context(|| format!("invalid config {}", path.display()))?;
        }

        if let Ok(value) = env::var("CODECRAB_MODEL") {
            config.model = value;
        }
        if let Ok(value) = env::var("CODECRAB_BASE_URL") {
            config.base_url = value;
        }
        if let Ok(value) = env::var("CODECRAB_API_KEY_ENV") {
            config.api_key_env = value;
        }
        if let Ok(value) = env::var("CODECRAB_AUTH") {
            config.auth = value;
        }
        Ok(config)
    }

    pub(crate) fn apply_cli(&mut self, model: Option<String>, base_url: Option<String>) {
        if let Some(value) = model {
            self.model = value;
        }
        if let Some(value) = base_url {
            self.base_url = value;
        }
    }

    pub(crate) fn api_key(&self) -> Result<Option<String>> {
        if self.api_key_env.trim().is_empty() {
            return Ok(None);
        }
        env::var(&self.api_key_env)
            .map(Some)
            .with_context(|| format!("environment variable {} is not set", self.api_key_env))
    }

    pub(crate) fn public_view(&self) -> PublicConfig<'_> {
        PublicConfig {
            model: &self.model,
            base_url: &self.base_url,
            auth: &self.auth,
            api_key_env: &self.api_key_env,
            request_timeout_seconds: self.request_timeout_seconds,
            session_directories: &self.session_directories,
        }
    }
}

impl SessionRegistry {
    pub(crate) fn global() -> Self {
        Self {
            path: Config::file_path(),
        }
    }

    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub(crate) fn directories(&self) -> Result<Vec<PathBuf>> {
        let Some(path) = &self.path else {
            return Ok(Vec::new());
        };
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text =
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))?;
        Ok(config.session_directories)
    }

    pub(crate) fn register(&self, root: &Path) -> Result<()> {
        let root = normalized_root(root);
        let mut directories = self.directories()?;
        if directories
            .iter()
            .any(|existing| paths_equal(existing, &root))
        {
            return Ok(());
        }
        directories.push(root);
        self.write_directories(&directories)
    }

    pub(crate) fn unregister(&self, root: &Path) -> Result<()> {
        let root = normalized_root(root);
        let mut directories = self.directories()?;
        let original_len = directories.len();
        directories.retain(|existing| !paths_equal(existing, &root));
        if directories.len() == original_len {
            return Ok(());
        }
        self.write_directories(&directories)
    }

    fn write_directories(&self, directories: &[PathBuf]) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut document = if path.exists() {
            fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?
                .parse::<DocumentMut>()
                .with_context(|| format!("invalid config {}", path.display()))?
        } else {
            DocumentMut::new()
        };
        let mut array = Array::new();
        for directory in directories {
            array.push(directory.to_string_lossy().as_ref());
        }
        document["session_directories"] = Item::Value(Value::Array(array));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        fs::write(path, document.to_string())
            .with_context(|| format!("cannot update {}", path.display()))
    }
}

pub(crate) fn normalized_root(root: &Path) -> PathBuf {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    #[cfg(windows)]
    {
        let text = canonical.to_string_lossy();
        if let Some(path) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{path}"));
        }
        if let Some(path) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(path);
        }
    }
    canonical
}

pub(crate) fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = normalized_root(left);
    let right = normalized_root(right);
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_registry_preserves_config_and_deduplicates_projects() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "# keep this comment\nmodel = \"auto\"\n").unwrap();
        let registry = SessionRegistry::at(path.clone());
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();

        registry.register(&project).unwrap();
        registry.register(&project).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"));
        assert_eq!(registry.directories().unwrap().len(), 1);

        registry.unregister(&project).unwrap();
        assert!(registry.directories().unwrap().is_empty());
    }
}
