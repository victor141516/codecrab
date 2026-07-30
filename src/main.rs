#![warn(unreachable_pub)]

mod agent;
mod audio;
mod auth;
mod compaction;
mod completion;
mod config;
mod conversation;
mod coordination;
mod diagnostics;
mod events;
mod http_debug;
mod project_fs;
mod provider;
mod server;
mod session;
mod skills;
mod terminal;
#[cfg(test)]
mod test_support;
mod tools;
mod transcription;
mod ui;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::{
    auth::OAuthStore,
    config::{Config, ConfigStore, ProviderConfig, SessionRegistry, validate_provider_name},
    coordination::SessionCoordinator,
    diagnostics::{DebugOutput, DiagnosticLog},
    session::{SessionStore, list_session_projects, resolve_global_session},
};

#[derive(Parser)]
#[command(
    name = "codecrab",
    version,
    about = "A small, auditable coding agent for your terminal"
)]
struct Cli {
    /// Project directory the agent may access.
    #[arg(short = 'C', long, global = true, default_value = ".")]
    cwd: PathBuf,

    /// Model name, overriding config and CODECRAB_MODEL.
    #[arg(short, long, global = true)]
    model: Option<String>,

    /// OpenAI-compatible API base URL.
    #[arg(long, global = true)]
    base_url: Option<String>,

    /// Print complete unredacted OpenAI HTTP traffic to stderr or an optional file.
    #[arg(
        long,
        global = true,
        num_args = 0..=1,
        require_equals = true,
        value_name = "PATH"
    )]
    debug_openai: Option<Option<PathBuf>>,

    /// Write interactive TUI error diagnostics to this file instead of a temporary file.
    #[arg(long, global = true, value_name = "PATH")]
    error_log: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Authenticate with a ChatGPT subscription or inspect auth state.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Add, inspect, select, or remove provider profiles.
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// Run one prompt, print the answer, and exit.
    Run {
        /// Prompt. If omitted, it is read from stdin.
        prompt: Option<String>,
    },
    /// Resume an existing interactive session.
    Resume {
        /// Session id or an unambiguous prefix. Omit for the latest session.
        id: Option<String>,
    },
    /// List saved sessions across every registered project.
    Sessions,
    /// List available project and user skills.
    Skills,
    /// Print the effective configuration (secrets are omitted).
    Config,
    /// Serve the embedded web application and agent API.
    Serve {
        /// Interface to listen on.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// TCP port. Use 0 to select an available port.
        #[arg(long, default_value_t = 4096)]
        port: u16,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Sign in with ChatGPT in your browser (Plus/Pro subscription).
    Login,
    /// Show the active authentication method without exposing credentials.
    Status,
    /// Remove CodeCrab's saved ChatGPT credentials.
    Logout,
}

#[derive(Subcommand)]
enum ProviderCommand {
    /// Add or replace a provider profile.
    Add {
        /// Profile name (letters, numbers, '-' and '_').
        name: String,
        /// OpenAI-compatible API base URL.
        #[arg(long)]
        base_url: String,
        /// Authentication mode: auto, oauth, api_key, or none.
        #[arg(long, default_value = "api_key")]
        auth: String,
        /// Default model name.
        #[arg(long, default_value = "auto")]
        model: String,
        /// API key. Prefer omitting this flag and entering it at the hidden prompt.
        #[arg(long, conflicts_with = "api_key_stdin")]
        api_key: Option<String>,
        /// Read the API key from stdin.
        #[arg(long)]
        api_key_stdin: bool,
        /// Make this profile active.
        #[arg(long)]
        activate: bool,
    },
    /// List provider profiles without exposing API keys.
    List,
    /// Show one provider profile without exposing its API key.
    Show { name: String },
    /// Select the provider used for new sessions.
    Use { name: String },
    /// Remove a provider profile.
    Remove { name: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let debug_openai = match cli.debug_openai {
        None => DebugOutput::default(),
        Some(None) => DebugOutput::stderr(),
        Some(Some(path)) => DebugOutput::file(path),
    };
    let root = cli
        .cwd
        .canonicalize()
        .with_context(|| format!("cannot open project {}", cli.cwd.display()))?;

    let mut config = Config::load()?;
    config.apply_cli(cli.model, cli.base_url)?;

    if let Some(Command::Auth { command }) = &cli.command {
        let mut auth = OAuthStore::new()?;
        auth.set_debug_openai(debug_openai.clone());
        match command {
            AuthCommand::Login => {
                let identity = auth.login().await?;
                println!(
                    "Signed in with ChatGPT{}{}.",
                    identity
                        .email
                        .as_deref()
                        .map(|email| format!(" as {email}"))
                        .unwrap_or_default(),
                    identity
                        .plan
                        .as_deref()
                        .map(|plan| format!(" ({plan})"))
                        .unwrap_or_default()
                );
            }
            AuthCommand::Status => match auth.status()? {
                Some(status) => println!(
                    "ChatGPT OAuth: signed in{}{}",
                    status
                        .email
                        .as_deref()
                        .map(|email| format!(" as {email}"))
                        .unwrap_or_default(),
                    status
                        .plan
                        .as_deref()
                        .map(|plan| format!(" ({plan})"))
                        .unwrap_or_default()
                ),
                None => println!(
                    "ChatGPT OAuth: not signed in\nRun `codecrab auth login` to use your subscription."
                ),
            },
            AuthCommand::Logout => {
                auth.logout()?;
                println!("CodeCrab's ChatGPT login was removed from the global configuration.");
            }
        }
        return Ok(());
    }

    if let Some(Command::Provider { command }) = &cli.command {
        manage_provider(command)?;
        return Ok(());
    }

    let registry = SessionRegistry::global()?;

    match cli.command {
        Some(Command::Sessions) => {
            ui::print_sessions(&list_session_projects(&root, &registry)?);
            return Ok(());
        }
        Some(Command::Skills) => {
            crate::skills::SkillRegistry::discover(&root).print();
            return Ok(());
        }
        Some(Command::Config) => {
            let contents = toml::to_string_pretty(&config.public_view())?;
            println!("{}", format_config_output(&Config::file_path()?, &contents));
            return Ok(());
        }
        Some(Command::Serve { host, port }) => {
            server::serve(root, config, host, port, debug_openai).await?;
            return Ok(());
        }
        Some(Command::Auth { .. } | Command::Provider { .. }) => {
            unreachable!("management commands return before session setup")
        }
        Some(Command::Run { prompt }) => {
            let prompt = one_shot_prompt(prompt)?;
            if prompt.trim().is_empty() {
                anyhow::bail!("prompt is empty");
            }
            let coordinator = SessionCoordinator::new(
                config.clone(),
                registry.clone(),
                debug_openai,
                DiagnosticLog::stderr(),
                Config::instructions_path()?,
            );
            let session = coordinator.create_session(&root)?;
            let mut agent = coordinator.build_agent(&root, session)?;
            match agent.fetch_models().await {
                Ok(catalog) => {
                    agent.resolve_new_session_model(&catalog);
                }
                Err(error) => {
                    return Err(error).context("cannot load the provider model catalog");
                }
            }
            let conversation = coordinator.install(agent)?;
            let conversations = coordinator.manager();
            let turn = conversation.turn(prompt.trim().to_owned()).await?;
            let result = turn.result;
            let shutdown = conversations.shutdown_all().await.map(|_| ());
            match (result, shutdown) {
                (Ok(answer), Ok(_)) => println!("{answer}"),
                (Ok(_), Err(error)) => return Err(error),
                (Err(error), Ok(_)) => return Err(error),
                (Err(error), Err(shutdown)) => {
                    return Err(error.context(format!(
                        "the conversation also could not shut down cleanly: {shutdown:#}"
                    )));
                }
            }
            return Ok(());
        }
        Some(Command::Resume { id }) => {
            let projects = list_session_projects(&root, &registry)?;
            let (session_root, session_id) = resolve_global_session(&projects, id.as_deref())?;
            let session_store = SessionStore::new(&session_root)?;
            let session = session_store.load(Some(&session_id.to_string()))?;
            let diagnostics = DiagnosticLog::tui(cli.error_log.clone());
            let coordinator = SessionCoordinator::new(
                config.clone(),
                registry.clone(),
                debug_openai.clone(),
                diagnostics.clone(),
                Config::instructions_path()?,
            );
            let agent = coordinator.build_agent(&session_root, session)?;
            ui::interactive(
                agent,
                &registry,
                debug_openai,
                diagnostics,
                config.clone(),
                false,
                coordinator,
            )
            .await?;
        }
        None => {
            let diagnostics = DiagnosticLog::tui(cli.error_log.clone());
            let coordinator = SessionCoordinator::new(
                config.clone(),
                registry.clone(),
                debug_openai.clone(),
                diagnostics.clone(),
                Config::instructions_path()?,
            );
            let session = coordinator.create_session(&root)?;
            let agent = coordinator.build_agent(&root, session)?;
            ui::interactive(
                agent,
                &registry,
                debug_openai,
                diagnostics,
                config.clone(),
                true,
                coordinator,
            )
            .await?;
        }
    }

    Ok(())
}

fn manage_provider(command: &ProviderCommand) -> Result<()> {
    use std::io::{IsTerminal, Read};

    let store = ConfigStore::global()?;
    let mut config = store.load()?;
    match command {
        ProviderCommand::Add {
            name,
            base_url,
            auth,
            model,
            api_key,
            api_key_stdin,
            activate,
        } => {
            validate_provider_name(name)?;
            let auth = auth.trim().to_ascii_lowercase();
            let needs_key = matches!(auth.as_str(), "auto" | "api_key");
            let key = if let Some(key) = api_key {
                key.clone()
            } else if *api_key_stdin {
                let mut key = String::new();
                std::io::stdin().read_to_string(&mut key)?;
                key.trim_end_matches(['\r', '\n']).to_owned()
            } else if needs_key && std::io::stdin().is_terminal() {
                rpassword::prompt_password("API key: ")?
            } else {
                String::new()
            };
            let provider = ProviderConfig {
                model: model.clone(),
                base_url: base_url.clone(),
                auth,
                api_key: key,
                ..config.providers.get(name).cloned().unwrap_or_default()
            };
            provider.validate(name)?;
            config.providers.insert(name.clone(), provider);
            if *activate || config.providers.len() == 1 {
                config.active_provider.clone_from(name);
            }
            store.save(&config)?;
            println!("Provider {name:?} saved in {}.", store.path().display());
        }
        ProviderCommand::List => {
            for provider in config.summaries() {
                println!(
                    "{}{}  {}  {}  key: {}",
                    if provider.active { "* " } else { "  " },
                    provider.name,
                    provider.model,
                    provider.base_url,
                    if provider.api_key_configured {
                        "configured"
                    } else {
                        "none"
                    }
                );
            }
        }
        ProviderCommand::Show { name } => {
            let provider = config.provider(name)?;
            println!("name: {name}");
            println!("active: {}", config.active_provider == *name);
            println!("base URL: {}", provider.base_url);
            println!("model: {}", provider.model);
            println!("auth: {}", provider.auth);
            println!(
                "API key: {}",
                if provider.api_key.is_empty() {
                    "not configured"
                } else {
                    "configured"
                }
            );
        }
        ProviderCommand::Use { name } => {
            config.provider(name)?;
            config.active_provider.clone_from(name);
            store.save(&config)?;
            println!("Provider {name:?} will be used for new sessions.");
        }
        ProviderCommand::Remove { name } => {
            config.provider(name)?;
            if config.active_provider == *name {
                anyhow::bail!("cannot remove the active provider; select another provider first");
            }
            config.providers.remove(name);
            store.save(&config)?;
            println!("Provider {name:?} removed.");
        }
    }
    Ok(())
}

fn one_shot_prompt(prompt: Option<String>) -> Result<String> {
    use std::io::{IsTerminal, Read};

    let stdin_is_terminal = std::io::stdin().is_terminal();
    if prompt.is_none() && stdin_is_terminal {
        anyhow::bail!("provide a prompt or pipe input to `codecrab run`");
    }

    let mut piped = String::new();
    if !stdin_is_terminal {
        std::io::stdin().read_to_string(&mut piped)?;
    }

    Ok(match (prompt, piped.trim()) {
        (Some(prompt), "") => prompt,
        (Some(prompt), input) => {
            format!("{prompt}\n\nThe following content was provided on stdin:\n\n{input}")
        }
        (None, input) => input.to_owned(),
    })
}

fn format_config_output(path: &Path, contents: &str) -> String {
    format!(
        "Configuration file path:\n{}\n\nEffective configuration content:\n{}",
        path.display(),
        contents.trim_end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_output_separates_the_file_path_from_effective_content() {
        let output = format_config_output(
            Path::new("/config/codecrab/config.toml"),
            "model = \"auto\"\n",
        );

        assert!(output.starts_with("Configuration file path:\n/config/codecrab/config.toml\n\n"));
        assert!(output.contains("Effective configuration content:\nmodel = \"auto\""));
    }

    #[test]
    fn debug_openai_accepts_no_output_path() {
        let cli = Cli::try_parse_from(["codecrab", "--debug-openai"]).unwrap();

        assert_eq!(cli.debug_openai, Some(None));
    }

    #[test]
    fn debug_openai_accepts_an_equals_separated_output_path() {
        let cli = Cli::try_parse_from(["codecrab", "--debug-openai=debug/openai.log"]).unwrap();

        assert_eq!(
            cli.debug_openai,
            Some(Some(PathBuf::from("debug/openai.log")))
        );
    }

    #[test]
    fn debug_openai_does_not_consume_a_subcommand() {
        let cli = Cli::try_parse_from(["codecrab", "--debug-openai", "run", "hello"]).unwrap();

        assert_eq!(cli.debug_openai, Some(None));
        assert!(matches!(
            cli.command,
            Some(Command::Run {
                prompt: Some(ref prompt)
            }) if prompt == "hello"
        ));
    }
}
