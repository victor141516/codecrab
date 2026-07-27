use std::{env, fs};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    pub model: String,
    pub base_url: String,
    pub auth: String,
    pub api_key_env: String,
    pub max_tool_rounds: usize,
    pub request_timeout_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "auto".into(),
            base_url: "https://api.openai.com/v1".into(),
            auth: "auto".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            max_tool_rounds: 24,
            request_timeout_seconds: 180,
        }
    }
}
#[derive(Serialize)]
pub(crate) struct PublicConfig<'a> {
    pub model: &'a str,
    pub base_url: &'a str,
    pub auth: &'a str,
    pub api_key_env: &'a str,
    pub max_tool_rounds: usize,
    pub request_timeout_seconds: u64,
}

impl Config {
    pub(crate) fn load() -> Result<Self> {
        let mut config = Self::default();
        if let Some(dirs) = ProjectDirs::from("", "", "codecrab") {
            let path = dirs.config_dir().join("config.toml");
            if path.exists() {
                let text = fs::read_to_string(&path)
                    .with_context(|| format!("cannot read {}", path.display()))?;
                config = toml::from_str(&text)
                    .with_context(|| format!("invalid config {}", path.display()))?;
            }
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
            max_tool_rounds: self.max_tool_rounds,
            request_timeout_seconds: self.request_timeout_seconds,
        }
    }
}
