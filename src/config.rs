use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, Item, Value};

pub(crate) const DEFAULT_PROVIDER: &str = "openai";
pub(crate) const OFFICIAL_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

fn global_config_mutation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct ProviderConfig {
    pub model: String,
    pub base_url: String,
    pub auth: String,
    pub api_key: String,
    pub fetch_models: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_models: Option<Vec<String>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub model_capabilities: BTreeMap<String, ModelCapabilitiesConfig>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ChatGptOAuthConfig {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub account_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct ModelCapabilitiesConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_reasoning_level: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasoning_levels: Vec<CatalogOptionConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_service_tier: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub service_tiers: Vec<CatalogOptionConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub output_modalities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_compact_token_limit: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct CatalogOptionConfig {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            model: "auto".into(),
            base_url: OFFICIAL_OPENAI_BASE_URL.into(),
            auth: "auto".into(),
            api_key: String::new(),
            fetch_models: true,
            allowed_models: None,
            model_capabilities: BTreeMap::new(),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chatgpt_oauth: Option<ChatGptOAuthConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            active_provider: DEFAULT_PROVIDER.into(),
            providers: BTreeMap::from([(DEFAULT_PROVIDER.into(), ProviderConfig::default())]),
            request_timeout_seconds: 180,
            session_directories: Vec::new(),
            chatgpt_oauth: None,
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
    pub fetch_models: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_models: Option<&'a [String]>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub model_capabilities: &'a BTreeMap<String, ModelCapabilitiesConfig>,
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
    mutation_lock: Arc<Mutex<()>>,
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
                            fetch_models: provider.fetch_models,
                            allowed_models: provider.allowed_models.as_deref(),
                            model_capabilities: &provider.model_capabilities,
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
    pub(crate) fn is_official_openai(&self) -> bool {
        self.base_url.trim_end_matches('/') == OFFICIAL_OPENAI_BASE_URL
    }

    #[cfg(test)]
    pub(crate) fn test(model: String, base_url: String) -> Self {
        Self {
            model,
            base_url,
            auth: "none".into(),
            api_key: String::new(),
            ..Self::default()
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
        if let Some(models) = &self.allowed_models {
            validate_unique_values(name, "allowed_models", models)?;
            if self.model != "auto" && !models.contains(&self.model) {
                anyhow::bail!(
                    "provider {name:?} selects model {:?}, but it is not present in allowed_models",
                    self.model
                );
            }
        }
        for (model_id, capabilities) in &self.model_capabilities {
            if model_id.trim().is_empty() {
                anyhow::bail!("provider {name:?} has an empty model capability identifier");
            }
            capabilities.validate(name, model_id)?;
        }
        Ok(())
    }
}

impl ModelCapabilitiesConfig {
    fn validate(&self, provider: &str, model: &str) -> Result<()> {
        if self
            .display_name
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            anyhow::bail!("provider {provider:?} model {model:?} has an empty display_name");
        }
        for (field, value) in [
            ("context_window_tokens", self.context_window_tokens),
            ("maximum_output_tokens", self.maximum_output_tokens),
            ("auto_compact_token_limit", self.auto_compact_token_limit),
        ] {
            if value == Some(0) {
                anyhow::bail!("provider {provider:?} model {model:?} configures {field} as zero");
            }
        }
        validate_options(provider, model, "reasoning_levels", &self.reasoning_levels)?;
        validate_options(provider, model, "service_tiers", &self.service_tiers)?;
        validate_unique_values(
            provider,
            &format!("model {model:?} input_modalities"),
            &self.input_modalities,
        )?;
        validate_unique_values(
            provider,
            &format!("model {model:?} output_modalities"),
            &self.output_modalities,
        )?;
        Ok(())
    }
}

fn validate_options(
    provider: &str,
    model: &str,
    field: &str,
    options: &[CatalogOptionConfig],
) -> Result<()> {
    let ids = options
        .iter()
        .map(|option| option.id.clone())
        .collect::<Vec<_>>();
    validate_unique_values(provider, &format!("model {model:?} {field}"), &ids)?;
    for option in options {
        if option
            .name
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            anyhow::bail!(
                "provider {provider:?} model {model:?} {field} option {:?} has an empty name",
                option.id
            );
        }
    }
    Ok(())
}

fn validate_unique_values(provider: &str, field: &str, values: &[String]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        if value.trim().is_empty() {
            anyhow::bail!("provider {provider:?} has an empty value in {field}");
        }
        if !seen.insert(value) {
            anyhow::bail!("provider {provider:?} has duplicate value {value:?} in {field}");
        }
    }
    Ok(())
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
        let _guard = global_config_mutation_lock()
            .lock()
            .expect("global configuration mutation lock poisoned");
        config.validate()?;
        let mut config = config.clone();
        if self.path.exists() {
            let text = fs::read_to_string(&self.path)
                .with_context(|| format!("cannot read {}", self.path.display()))?;
            let current: Config = toml::from_str(&text)
                .with_context(|| format!("invalid config {}", self.path.display()))?;
            config.session_directories = current.session_directories;
            config.chatgpt_oauth = current.chatgpt_oauth;
        }
        self.write(&config)
    }

    pub(crate) fn set_chatgpt_oauth(&self, oauth: Option<ChatGptOAuthConfig>) -> Result<()> {
        let _guard = global_config_mutation_lock()
            .lock()
            .expect("global configuration mutation lock poisoned");
        let mut config = self.load()?;
        config.chatgpt_oauth = oauth;
        self.write(&config)
    }

    fn write(&self, config: &Config) -> Result<()> {
        config.validate()?;
        let parent = self
            .path
            .parent()
            .context("configuration path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        let temp = self.path.with_extension("toml.tmp");
        fs::write(&temp, toml::to_string_pretty(&config)?)
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
            mutation_lock: Arc::new(Mutex::new(())),
        }
    }

    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            mutation_lock: Arc::new(Mutex::new(())),
        }
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
        let _guard = self
            .mutation_lock
            .lock()
            .expect("session registry mutation lock poisoned");
        let _global_guard = global_config_mutation_lock()
            .lock()
            .expect("global configuration mutation lock poisoned");
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
        let _guard = self
            .mutation_lock
            .lock()
            .expect("session registry mutation lock poisoned");
        let _global_guard = global_config_mutation_lock()
            .lock()
            .expect("global configuration mutation lock poisoned");
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
        let temp = path.with_extension("toml.registry.tmp");
        fs::write(&temp, document.to_string())
            .with_context(|| format!("cannot write {}", temp.display()))?;
        if path.exists() {
            fs::remove_file(path).with_context(|| format!("cannot replace {}", path.display()))?;
        }
        fs::rename(&temp, path).with_context(|| format!("cannot update {}", path.display()))
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
        let oauth = ChatGptOAuthConfig {
            access_token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
            expires_at: 123,
            account_id: "account-id".into(),
            email: Some("user@example.com".into()),
            plan: Some("Pro".into()),
        };
        config.chatgpt_oauth = Some(oauth.clone());
        store.save(&config).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(
            loaded.providers.get(DEFAULT_PROVIDER).unwrap().api_key,
            "sk-secret"
        );
        assert_eq!(loaded.chatgpt_oauth, Some(oauth));
        let public = toml::to_string(&config.public_view()).unwrap();
        assert!(public.contains("<redacted>"));
        assert!(!public.contains("sk-secret"));
        assert!(!public.contains("access-secret"));
        assert!(!public.contains("refresh-secret"));
        assert!(!public.contains("chatgpt_oauth"));
    }

    #[test]
    fn parses_and_validates_manual_model_catalog() {
        let config: Config = toml::from_str(
            r#"
active_provider = "local"

[providers.local]
model = "auto"
base_url = "http://localhost:1234/v1"
auth = "none"
fetch_models = false
allowed_models = ["vision-model"]

[providers.local.model_capabilities."vision-model"]
display_name = "Vision Model"
default_reasoning_level = "high"
input_modalities = ["text", "image"]
output_modalities = ["text"]
reasoning_levels = [{ id = "high", name = "Deep" }]
service_tiers = [{ id = "priority" }]
"#,
        )
        .unwrap();

        config.validate().unwrap();
        let provider = config.provider("local").unwrap();
        assert!(!provider.fetch_models);
        assert_eq!(
            provider.allowed_models.as_deref().unwrap(),
            ["vision-model"]
        );
        let model = &provider.model_capabilities["vision-model"];
        assert_eq!(model.reasoning_levels[0].name.as_deref(), Some("Deep"));
        assert_eq!(model.service_tiers[0].name, None);
    }

    #[test]
    fn official_openai_detection_is_based_on_the_provider_base_url() {
        let mut provider = ProviderConfig::default();
        assert!(provider.is_official_openai());
        provider.base_url.push('/');
        assert!(provider.is_official_openai());
        provider.base_url = "https://provider.example/v1".into();
        assert!(!provider.is_official_openai());
    }

    #[test]
    fn rejects_explicit_default_outside_allowed_models() {
        let mut config = Config::default();
        let provider = config.providers.get_mut(DEFAULT_PROVIDER).unwrap();
        provider.model = "other-model".into();
        provider.allowed_models = Some(vec!["only-model".into()]);

        assert!(format!("{:#}", config.validate().unwrap_err()).contains("allowed_models"));
    }

    #[test]
    fn rejects_duplicate_manual_option_ids() {
        let mut config = Config::default();
        config
            .providers
            .get_mut(DEFAULT_PROVIDER)
            .unwrap()
            .model_capabilities
            .insert(
                "model".into(),
                ModelCapabilitiesConfig {
                    reasoning_levels: vec![
                        CatalogOptionConfig {
                            id: "high".into(),
                            name: None,
                            description: None,
                        },
                        CatalogOptionConfig {
                            id: "high".into(),
                            name: None,
                            description: None,
                        },
                    ],
                    ..ModelCapabilitiesConfig::default()
                },
            );

        assert!(format!("{:#}", config.validate().unwrap_err()).contains("duplicate"));
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

    #[test]
    fn stale_provider_save_cannot_erase_a_concurrent_session_registration() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let store = ConfigStore::new(path.clone());
        store.save(&Config::default()).unwrap();
        let stale = store.load().unwrap();
        let registry = SessionRegistry::at(path);
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let oauth = ChatGptOAuthConfig {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: 123,
            account_id: "account".into(),
            email: None,
            plan: None,
        };

        store.set_chatgpt_oauth(Some(oauth.clone())).unwrap();
        registry.register(&project).unwrap();
        store.save(&stale).unwrap();

        assert_eq!(
            registry.directories().unwrap(),
            vec![normalized_root(&project)]
        );
        assert_eq!(store.load().unwrap().chatgpt_oauth, Some(oauth));
    }
}
