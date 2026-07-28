use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, Item, Value};

pub(crate) const DEFAULT_PROVIDER: &str = "openai";

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct ProviderConfig {
    pub model: String,
    pub base_url: String,
    pub auth: String,
    pub api_key: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            model: "auto".into(),
            base_url: "https://api.openai.com/v1".into(),
            auth: "auto".into(),
            api_key: String::new(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Config {
    pub active_provider: String,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub request_timeout_seconds: u64,
    pub session_directories: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            active_provider: DEFAULT_PROVIDER.into(),
            providers: BTreeMap::from([(DEFAULT_PROVIDER.into(), ProviderConfig::default())]),
            request_timeout_seconds: 180,
            session_directories: Vec::new(),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct PublicConfig<'a> {
    pub active_provider: &'a str,
    pub providers: BTreeMap<&'a str, PublicProvider<'a>>,
    pub request_timeout_seconds: u64,
    pub session_directories: &'a [PathBuf],
}

#[derive(Serialize)]
pub(crate) struct PublicProvider<'a> {
    pub model: &'a str,
    pub base_url: &'a str,
    pub auth: &'a str,
    pub api_key: &'static str,
}

#[derive(Clone, Serialize)]
pub(crate) struct ProviderSummary {
    pub name: String,
    pub model: String,
    pub base_url: String,
    pub auth: String,
    pub api_key_configured: bool,
    pub active: bool,
}

#[derive(Clone)]
pub(crate) struct ConfigStore {
    path: PathBuf,
}

#[derive(Clone)]
pub(crate) struct SessionRegistry {
    path: Option<PathBuf>,
}

impl Config {
    #[cfg(test)]
    pub(crate) fn test(model: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            providers: BTreeMap::from([(
                DEFAULT_PROVIDER.into(),
                ProviderConfig::test(model.into(), base_url.into()),
            )]),
            ..Self::default()
        }
    }

    pub(crate) fn file_path() -> Option<PathBuf> {
        ProjectDirs::from("", "", "codecrab").map(|dirs| dirs.config_dir().join("config.toml"))
    }

    pub(crate) fn load() -> Result<Self> {
        let mut config = if let Some(path) = Self::file_path().filter(|path| path.exists()) {
            ConfigStore::new(path).load()?
        } else {
            Self::default()
        };

        if let Ok(value) = env::var("CODECRAB_PROVIDER") {
            config.active_provider = value;
        }
        let active = config.active_provider.clone();
        if let Some(provider) = config.providers.get_mut(&active) {
            if let Ok(value) = env::var("CODECRAB_MODEL") {
                provider.model = value;
            }
            if let Ok(value) = env::var("CODECRAB_BASE_URL") {
                provider.base_url = value;
            }
            if let Ok(value) = env::var("CODECRAB_AUTH") {
                provider.auth = value;
            }
            if let Ok(value) = env::var("CODECRAB_API_KEY") {
                provider.api_key = value;
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn apply_cli(
        &mut self,
        model: Option<String>,
        base_url: Option<String>,
    ) -> Result<()> {
        let active = self.active_provider.clone();
        let provider = self.provider_mut(&active)?;
        if let Some(value) = model {
            provider.model = value;
        }
        if let Some(value) = base_url {
            provider.base_url = value;
        }
        Ok(())
    }

    pub(crate) fn provider(&self, name: &str) -> Result<&ProviderConfig> {
        self.providers
            .get(name)
            .with_context(|| format!("provider {name:?} is not configured"))
    }

    pub(crate) fn provider_mut(&mut self, name: &str) -> Result<&mut ProviderConfig> {
        self.providers
            .get_mut(name)
            .with_context(|| format!("provider {name:?} is not configured"))
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.request_timeout_seconds == 0 {
            anyhow::bail!("request_timeout_seconds must be greater than zero");
        }
        validate_provider_name(&self.active_provider)?;
        self.provider(&self.active_provider)?;
        if self.providers.is_empty() {
            anyhow::bail!("at least one provider must be configured");
        }
        for (name, provider) in &self.providers {
            validate_provider_name(name)?;
            provider.validate(name)?;
        }
        Ok(())
    }

    pub(crate) fn summaries(&self) -> Vec<ProviderSummary> {
        self.providers
            .iter()
            .map(|(name, provider)| ProviderSummary {
                name: name.clone(),
                model: provider.model.clone(),
                base_url: provider.base_url.clone(),
                auth: provider.auth.clone(),
                api_key_configured: !provider.api_key.is_empty(),
                active: *name == self.active_provider,
            })
            .collect()
    }

    pub(crate) fn public_view(&self) -> PublicConfig<'_> {
        PublicConfig {
            active_provider: &self.active_provider,
            providers: self
                .providers
                .iter()
                .map(|(name, provider)| {
                    (
                        name.as_str(),
                        PublicProvider {
                            model: &provider.model,
                            base_url: &provider.base_url,
                            auth: &provider.auth,
                            api_key: if provider.api_key.is_empty() {
                                ""
                            } else {
                                "<redacted>"
                            },
                        },
                    )
                })
                .collect(),
            request_timeout_seconds: self.request_timeout_seconds,
            session_directories: &self.session_directories,
        }
    }
}

impl ProviderConfig {
    #[cfg(test)]
    pub(crate) fn test(model: String, base_url: String) -> Self {
        Self {
            model,
            base_url,
            auth: "none".into(),
            api_key: String::new(),
        }
    }

    pub(crate) fn validate(&self, name: &str) -> Result<()> {
        if self.model.trim().is_empty() {
            anyhow::bail!("provider {name:?} has an empty model");
        }
        if self.base_url.trim().is_empty() {
            anyhow::bail!("provider {name:?} has an empty base_url");
        }
        match self.auth.trim().to_ascii_lowercase().as_str() {
            "auto" | "oauth" | "api_key" | "none" => {}
            other => anyhow::bail!(
                "provider {name:?} has invalid auth mode {other:?}; expected auto, oauth, api_key, or none"
            ),
        }
        Ok(())
    }
}

pub(crate) fn validate_provider_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        anyhow::bail!(
            "invalid provider name {name:?}; use only ASCII letters, numbers, '-' and '_'"
        );
    }
    Ok(())
}

impl ConfigStore {
    pub(crate) fn global() -> Result<Self> {
        Ok(Self::new(
            Config::file_path().context("platform configuration path is unavailable")?,
        ))
    }

    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn load(&self) -> Result<Config> {
        if !self.path.exists() {
            return Ok(Config::default());
        }
        let text = fs::read_to_string(&self.path)
            .with_context(|| format!("cannot read {}", self.path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("invalid config {}", self.path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn save(&self, config: &Config) -> Result<()> {
        config.validate()?;
        let parent = self
            .path
            .parent()
            .context("configuration path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        let temp = self.path.with_extension("toml.tmp");
        fs::write(&temp, toml::to_string_pretty(config)?)
            .with_context(|| format!("cannot write {}", temp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
        }
        if self.path.exists() {
            fs::remove_file(&self.path)
                .with_context(|| format!("cannot replace {}", self.path.display()))?;
        }
        fs::rename(&temp, &self.path)
            .with_context(|| format!("cannot save {}", self.path.display()))
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
    fn config_store_round_trips_secrets_but_public_view_redacts_them() {
        let temp = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(temp.path().join("config.toml"));
        let mut config = Config::default();
        config.providers.get_mut(DEFAULT_PROVIDER).unwrap().api_key = "sk-secret".into();
        store.save(&config).unwrap();

        assert_eq!(
            store
                .load()
                .unwrap()
                .providers
                .get(DEFAULT_PROVIDER)
                .unwrap()
                .api_key,
            "sk-secret"
        );
        let public = toml::to_string(&config.public_view()).unwrap();
        assert!(public.contains("<redacted>"));
        assert!(!public.contains("sk-secret"));
    }

    #[test]
    fn session_registry_preserves_config_and_deduplicates_projects() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "# keep this comment\nactive_provider = \"openai\"\n").unwrap();
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
